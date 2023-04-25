use std::{
    error::Error,
    io::{BufRead, Seek, SeekFrom},
};

use crate::{
    util::{has_data_left, read_bytes_with_len, read_f32, read_i32, read_u32},
    ContainerType, ElementType,
};

#[derive(Debug, thiserror::Error)]
pub enum LoadError<E: Error> {
    #[error("invalid file magic number: {0}")]
    InvalidMagic(u32),

    #[error("invalid ggml format: format={0:?} version={1}")]
    InvalidFormatVersion(ContainerType, u32),

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    FailedCast(#[from] std::num::TryFromIntError),

    #[error("implementation returned error: {0}")]
    ImplementationError(E),

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
    pub element_type: ElementType,
    /// start of tensor - start of file
    pub start_offset: u64,
}
impl TensorInfo {
    pub fn calc_size(&self) -> usize {
        let mut size = ggml::type_size(self.element_type);
        for &dim in &self.dims[0..self.n_dims] {
            size *= dim;
        }
        size / ggml::blck_size(self.element_type)
    }
}

/// Info in hyperparameter used for later loading tasks. Used in callback.
/// see [`LoadHandler::load_hyper_parameters`]
#[derive(Debug, Clone)]
pub struct PartialHyperparameters {
    pub n_vocab: usize,
}

pub trait LoadHandler<E: Error, R: BufRead + Seek> {
    /// Called when the container type is read.
    fn container_type(&mut self, container_type: ContainerType) -> Result<(), E>;
    /// Called when a vocabulary token is read.
    fn vocabulary_token(&mut self, i: usize, token: Vec<u8>, score: f32) -> Result<(), E>;
    /// Called when the hyperparameters need to be read.
    /// You must read the hyperparameters for your model here.
    fn read_hyperparameters(&mut self, reader: &mut R) -> Result<PartialHyperparameters, E>;
    /// Called when a new tensor is found.
    fn tensor_buffer(&mut self, info: TensorInfo) -> Result<(), E>;
}

#[test]
fn can_be_vtable() {
    use std::mem::MaybeUninit;
    let _a: MaybeUninit<Box<dyn LoadHandler<(), std::fs::File>>> = MaybeUninit::uninit();
}

pub fn load_model_from_reader<E: Error, R: BufRead + Seek>(
    reader: &mut R,
    handler: &mut impl LoadHandler<E, R>,
) -> Result<(), LoadError<E>> {
    // Verify magic
    let container_type: ContainerType = match read_u32(reader)? {
        ggml::FILE_MAGIC_GGMF => ContainerType::GGMF,
        ggml::FILE_MAGIC_GGJT => ContainerType::GGJT,
        ggml::FILE_MAGIC_UNVERSIONED => ContainerType::GGML,
        magic => return Err(LoadError::InvalidMagic(magic)),
    };
    handler
        .container_type(container_type)
        .map_err(LoadError::ImplementationError)?;

    // Load format version
    match container_type {
        ContainerType::GGMF | ContainerType::GGJT => {
            let _version: u32 = match read_u32(reader)? {
                ggml::FORMAT_VERSION => ggml::FORMAT_VERSION,
                version => return Err(LoadError::InvalidFormatVersion(container_type, version)),
            };
        }
        ContainerType::GGML => {}
    }

    // Load hyper params
    let hparams = handler
        .read_hyperparameters(reader)
        .map_err(LoadError::ImplementationError)?;
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
        handler
            .vocabulary_token(i, token, token_score)
            .map_err(LoadError::ImplementationError)?;
    }

    // Load tensor data
    match container_type {
        ContainerType::GGMF | ContainerType::GGML => load_weights(reader, handler, false),
        ContainerType::GGJT => load_weights(reader, handler, true),
    }
}

/// # Params
///
/// `align`
/// align to 4 bytes before reading tensor weights
fn load_weights<E: Error, R: BufRead + Seek>(
    reader: &mut R,
    handler: &mut impl LoadHandler<E, R>,
    align: bool,
) -> Result<(), LoadError<E>> {
    while has_data_left(reader)? {
        // load tensor header
        let n_dims: usize = read_i32(reader)?.try_into()?;
        let name_len = read_i32(reader)?;
        let ftype = read_i32(reader)?;
        let ftype =
            ggml::Type::try_from(ftype).map_err(|_| LoadError::UnsupportedElementType(ftype))?;

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
            element_type: ftype,
            start_offset: offset_aligned,
        };
        let n_bytes = tensor_info.calc_size();
        handler
            .tensor_buffer(tensor_info)
            .map_err(LoadError::ImplementationError)?;
        reader.seek(SeekFrom::Start(offset_aligned + n_bytes as u64))?;
    }

    Ok(())
}
