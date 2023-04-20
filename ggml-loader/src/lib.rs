//! standalone model loader
//!
//! Only the hyperparameter is llama-specific. Everything else can be reused for other LLM.
#![allow(clippy::nonminimal_bool)]

pub mod util;

use std::ops::ControlFlow;
use util::*;

pub type ElementType = ggml::Type;

/// file type containing the model
#[derive(Debug, PartialEq, Clone, Copy)]
#[allow(clippy::upper_case_acronyms)]
pub enum ContainerType {
    /// legacy format, oldest ggml tensor file format
    GGML,
    /// also legacy format, newer than GGML, older than GGJT
    GGMF,
    /// mmap-able format
    GGJT,
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError<T> {
    #[error("invalid file magic number: {0}")]
    InvalidMagic(u32),

    #[error("invalid ggml format: version={0}")]
    InvalidFormatVersion(u32),

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    FailedCast(#[from] std::num::TryFromIntError),

    /// return `ControlFlow::Break` from any of the `cb_*` function to trigger this error
    #[error("user requested interrupt: {0}")]
    UserInterrupted(T),

    #[error("unsupported tensor dtype/f16_: {0}")]
    UnsupportedElementType(i32),

    /// sanity check failed
    #[error("invariant broken: {0}")]
    InvariantBroken(String),
}

#[derive(Debug, Clone)]
pub struct TensorInfo {
    pub name: Vec<u8>,
    pub n_dims: usize,
    pub dims: [usize; 2],
    pub n_elements: usize,
    pub ftype: ElementType,
    /// start of tensor - start of file
    pub start_offset: u64,
}

/// Info in hyperparameter used for later loading tasks. Used in callback.
/// see [`LoadHandler::load_hyper_parameters`]
#[derive(Debug, Clone)]
pub struct PartialHyperparameters {
    pub n_vocab: usize,
}

#[allow(unused_variables)]
pub trait LoadHandler<T, R: BufRead + Seek> {
    fn got_container_type(&mut self, model_type: ContainerType) -> ControlFlow<T> {
        ControlFlow::Continue(())
    }

    fn got_vocab_token(&mut self, i: usize, token: Vec<u8>, score: f32) -> ControlFlow<T> {
        ControlFlow::Continue(())
    }

    fn load_hyper_parameters(&mut self, reader: &mut R) -> ControlFlow<T, PartialHyperparameters>;

    /// multi-file loading is not supported
    /// To handle that yourself, return [`ControlFlow::Break(_)`] here
    fn load_multipart(&mut self, reader: &mut R) -> ControlFlow<T> {
        ControlFlow::Continue(())
    }

    /// callback to get tensor buffer to populate
    ///
    /// # Returns
    ///
    /// `None` to skip copying
    /// `Some(buf)` to provide a buffer for copying weights into
    fn tensor_buffer(&mut self, info: TensorInfo) -> ControlFlow<T, Option<&mut [u8]>> {
        ControlFlow::Continue(None)
    }
}

#[test]
fn can_be_vtable() {
    use std::mem::MaybeUninit;
    let _a: MaybeUninit<Box<dyn LoadHandler<(), std::fs::File>>> = MaybeUninit::uninit();
}

pub fn load_model_from_reader<T, R: BufRead + Seek>(
    reader: &mut R,
    handler: &mut impl LoadHandler<T, R>,
) -> Result<(), LoadError<T>> {
    // Verify magic
    let container_type: ContainerType = match read_u32(reader)? {
        ggml::FILE_MAGIC_GGMF => ContainerType::GGMF,
        ggml::FILE_MAGIC_GGJT => ContainerType::GGJT,
        ggml::FILE_MAGIC_UNVERSIONED => ContainerType::GGML,
        magic => return Err(LoadError::InvalidMagic(magic)),
    };
    retchk(handler.got_container_type(container_type))?;

    // Load format version
    match container_type {
        ContainerType::GGMF | ContainerType::GGJT => {
            let _version: u32 = match read_u32(reader)? {
                ggml::FORMAT_VERSION => ggml::FORMAT_VERSION,
                version => return Err(LoadError::InvalidFormatVersion(version)),
            };
        }
        ContainerType::GGML => {}
    }

    // Load hyper params
    let hparams = retchk(handler.load_hyper_parameters(reader))?;
    let n_vocab = hparams.n_vocab;

    // Load vocabulary
    for i in 0..n_vocab {
        let len = read_u32(reader)?.try_into()?;
        let token = read_bytes_with_len(reader, len)?;
        let token_score = match container_type {
            ContainerType::GGMF | ContainerType::GGJT => read_f32(reader)?,
            ContainerType::GGML => {
                // Legacy model, set empty score
                0.
            }
        };
        retchk(handler.got_vocab_token(i, token, token_score))?;
    }

    // Load tensor data
    match container_type {
        ContainerType::GGMF | ContainerType::GGML => {
            retchk(handler.load_multipart(reader))?;
            load_weights(reader, handler, false)
        }
        ContainerType::GGJT => load_weights_ggjt(reader, handler),
    }
}

pub fn load_weights_ggjt<T, R: BufRead + Seek>(
    reader: &mut R,
    handler: &mut impl LoadHandler<T, R>,
) -> Result<(), LoadError<T>> {
    load_weights(reader, handler, true)
}

/// # Params
///
/// `align`
/// align to 4 bytes before reading tensor weights
pub fn load_weights<T, R: BufRead + Seek>(
    reader: &mut R,
    handler: &mut impl LoadHandler<T, R>,
    align: bool,
) -> Result<(), LoadError<T>> {
    while has_data_left(reader)? {
        // load tensor header
        let n_dims: usize = read_i32(reader)?.try_into()?;
        let name_len = read_i32(reader)?;
        let ftype = decode_element_type_res(read_i32(reader)?)?;

        let mut n_elements: usize = 1;
        let mut dims = [1usize, 1];
        let ne_len = dims.len();
        if !(n_dims <= ne_len) {
            return Err(LoadError::InvariantBroken(format!("{n_dims} <= {ne_len}")));
        }
        #[allow(clippy::needless_range_loop)]
        for i in 0..n_dims {
            let dim: usize = read_i32(reader)?.try_into()?;
            dims[i] = dim;
            n_elements *= dim;
        }

        // load tensor name
        let name = read_bytes_with_len(reader, name_len.try_into()?)?;

        // sanity check
        match ftype {
            ElementType::Q4_0 | ElementType::Q4_1 => {
                if !(dims[0] % 64 == 0) {
                    return Err(LoadError::InvariantBroken(format!("{dims:?}[0] % 64 == 0")));
                }
            }
            _ => {}
        }

        // load tensor weights
        let offset_curr = reader.stream_position()?;
        let offset_aligned: u64 = if align {
            (offset_curr + 31) & !31
        } else {
            offset_curr
        };

        let tensor_info = TensorInfo {
            name,
            dims,
            n_dims,
            n_elements,
            ftype,
            start_offset: offset_aligned,
        };

        let type_size = ggml::type_size(ftype);
        if let Some(buf) = retchk(handler.tensor_buffer(tensor_info))? {
            if align {
                reader.seek(SeekFrom::Start(offset_aligned))?;
            }
            let buf_len = buf.len();
            if !(buf_len == type_size * n_elements) {
                return Err(LoadError::InvariantBroken(format!(
                    "{buf_len} == {type_size} * {n_elements}"
                )));
            }
            reader.read_exact(buf)?;
        } else {
            // skip if no buffer is given
            reader.seek(SeekFrom::Start(
                offset_aligned + (type_size * n_elements) as u64,
            ))?;
        }
    }

    Ok(())
}
