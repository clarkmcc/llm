use crate::ggml::{
    quantize_q4_0, quantize_q4_1, FILE_MAGIC, FILE_MAGIC_UNVERSIONED, FORMAT_VERSION, TYPE_Q4_0,
    TYPE_Q4_1,
};
use crate::{Hyperparameters, LoadError, Vocabulary};
use half::f16;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use thiserror::Error;

const FTYPE_STR: [&str; 4] = ["f32", "f16", "q4_0", "q4_1"];

pub fn llama_model_quantize(
    file_name_in: impl AsRef<Path>,
    file_name_out: impl AsRef<Path>,
    itype: u8,
    qk: u8,
) -> Result<bool, LoadError> {
    let mut otype = TYPE_Q4_1;

    match itype {
        2 => otype = TYPE_Q4_0,
        3 => otype = TYPE_Q4_1,
        _ => {
            return Err(LoadError::InvalidItype(itype));
        }
    };

    let file_in = file_name_in.as_ref();
    let mut finp = BufReader::new(File::open(file_in).map_err(|e| LoadError::OpenFileFailed {
        source: e,
        path: file_in.to_owned(),
    })?);

    let file_out = file_name_out.as_ref();
    let mut fout =
        BufWriter::new(
            File::create(file_out).map_err(|e| LoadError::CreateFileFailed {
                source: e,
                path: file_out.to_owned(),
            })?,
        );

    // Verify magic
    {
        let mut magic_buffer: [u8; 4] = [0; 4];
        finp.read_exact(&mut magic_buffer).unwrap();

        let magic = u32::from_le_bytes(magic_buffer);
        if magic == FILE_MAGIC_UNVERSIONED {
            return Err(LoadError::UnversionedMagic);
        }
        if magic != FILE_MAGIC {
            return Err(LoadError::InvalidMagic {
                path: file_in.to_owned(),
            });
        }

        fout.write(&magic_buffer).unwrap();

        let mut version_buffer: [u8; 4] = [0; 4];
        finp.read_exact(&mut version_buffer).unwrap();

        let format_version = u32::from_le_bytes(version_buffer);

        if format_version != FORMAT_VERSION {
            return Err(LoadError::InvalidFormatVersion {
                value: format_version,
            });
        }

        fout.write(&version_buffer).unwrap();
    }

    let mut hparams = Hyperparameters::default();

    // Load parameters
    {
        let mut buffer: [u8; 4] = [0; 4];
        finp.read_exact(&mut buffer).unwrap();
        hparams.n_vocab = i32::from_le_bytes(buffer);
        println!("n_vocab: {}", hparams.n_vocab);
        fout.write(&buffer).unwrap();

        finp.read_exact(&mut buffer).unwrap();
        hparams.n_embd = i32::from_le_bytes(buffer);
        println!("n_embd: {}", hparams.n_embd);
        fout.write(&buffer).unwrap();

        finp.read_exact(&mut buffer).unwrap();
        hparams.n_mult = i32::from_le_bytes(buffer);
        println!("n_mult: {}", hparams.n_mult);
        fout.write(&buffer).unwrap();

        finp.read_exact(&mut buffer).unwrap();
        hparams.n_head = i32::from_le_bytes(buffer);
        println!("n_head: {}", hparams.n_head);
        fout.write(&buffer).unwrap();

        finp.read_exact(&mut buffer).unwrap();
        hparams.n_layer = i32::from_le_bytes(buffer);
        println!("n_layer: {}", hparams.n_layer);
        fout.write(&buffer).unwrap();

        finp.read_exact(&mut buffer).unwrap();
        hparams.n_rot = i32::from_le_bytes(buffer);
        println!("n_rot: {}", hparams.n_rot);
        fout.write(&buffer).unwrap();

        finp.read_exact(&mut buffer).unwrap();
        hparams.f16_ = i32::from_le_bytes(buffer);
        println!("f16_: {}", hparams.f16_);
        fout.write(&(itype as i32).to_le_bytes()).unwrap();
    }

    // load vocab
    let mut vocab = Vocabulary {
        id_to_token: vec![],
        id_to_token_score: vec![],
        token_to_id: Default::default(),
        max_token_length: 0,
    };

    {
        let n_vocab = hparams.n_vocab;

        for i in 0..n_vocab {
            let mut len_buffer = [0u8; 4];
            finp.read_exact(&mut len_buffer).unwrap();
            fout.write(&len_buffer).unwrap();
            let len = u32::from_le_bytes(len_buffer) as usize;

            let mut word_buffer = vec![0u8; len];
            finp.read_exact(word_buffer.as_mut_slice()).unwrap();
            fout.write(&word_buffer).unwrap();

            let word = String::from_utf8_lossy(&word_buffer).to_string();

            let mut score_buffer = [0u8; 4];
            finp.read_exact(&mut score_buffer).unwrap();
            fout.write(&score_buffer).unwrap();
            let score = f32::from_le_bytes(score_buffer);

            vocab.token_to_id.insert(word.clone(), i);

            vocab.id_to_token.push(word);
            vocab.id_to_token_score.push(score);
        }
    }

    // Load weights
    {
        let mut total_size_org: usize = 0;
        let mut total_size_new: usize = 0;

        let mut work: Vec<f32> = vec![];

        let mut data_u8: Vec<u8> = vec![];
        let mut data_f16: Vec<u16> = vec![];
        let mut data_f32: Vec<f32> = vec![];

        let mut hist_all: Vec<i64> = vec![0; 16];

        loop {
            let mut buffer = [0u8; 4];
            if finp.read_exact(&mut buffer).is_err() {
                break;
            };
            let n_dims = i32::from_le_bytes(buffer);

            if finp.read_exact(&mut buffer).is_err() {
                break;
            };
            let length = i32::from_le_bytes(buffer) as usize;

            if finp.read_exact(&mut buffer).is_err() {
                break;
            };
            let mut ftype = i32::from_le_bytes(buffer) as usize;

            println!("n_dims: {}, length: {}, ftype: {} ", n_dims, length, ftype);

            let mut nelements = 1i32;
            let mut ne = [1i32, 1i32];
            for i in 0..n_dims {
                finp.read_exact(&mut buffer).unwrap();
                ne[i as usize] = i32::from_le_bytes(buffer);
                nelements *= ne[i as usize];
            }

            let mut name_buffer = vec![0u8; length];
            finp.read_exact(&mut name_buffer).unwrap();
            let name = String::from_utf8(name_buffer).unwrap();
            println!("Nelements: {}", nelements);
            print!(
                "{:>48} - [{:>5}, {:>5}], type = {:>6}",
                format!("'{}'", name),
                ne[0],
                ne[1],
                FTYPE_STR[ftype]
            );

            // Quantize only 2D tensors
            let mut quantize = name.find("weight").is_some() && n_dims == 2;

            if quantize {
                if ftype != 0 && ftype != 1 {
                    return Err(LoadError::InvalidFtype {
                        ftype: ftype as i32,
                        path: file_in.to_owned(),
                    });
                }

                data_f32.resize(nelements as usize, 0.0);
                if ftype == 1 {
                    data_f16.resize(nelements as usize, 0);

                    let mut buffer = vec![0u8; (nelements * 2) as usize];
                    finp.read_exact(&mut buffer).unwrap();
                    // Compute buffer
                    for (index, chunk) in buffer.chunks(2).enumerate() {
                        let i = u16::from_le_bytes([chunk[0], chunk[1]]);
                        data_f16[index] = i;

                        //data_f32[index] = ggml_fp16_to_fp32(i);
                        data_f32[index] = f16::from_bits(i).to_f32();
                    }
                } else {
                    let mut buffer = vec![0u8; (nelements * 4) as usize];
                    finp.read_exact(&mut buffer).unwrap();

                    for (index, chunk) in buffer.chunks(4).enumerate() {
                        data_f32[index] =
                            f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    }
                }

                ftype = itype as usize;
            } else {
                // Determines the total bytes were dealing with
                let bpe = (nelements * if ftype == 0 { 4 } else { 2 }) as usize;

                data_u8.resize(bpe, 0);
                finp.read_exact(&mut data_u8).unwrap();
            }

            // Write data
            fout.write(&n_dims.to_le_bytes()).unwrap();
            fout.write(&(length as i32).to_le_bytes()).unwrap();
            println!(" new ftype: {}", ftype);
            println!("{:?}", name.as_bytes());
            fout.write(&(ftype as i32).to_le_bytes()).unwrap();

            for i in 0..n_dims {
                fout.write(&ne[i as usize].to_le_bytes()).unwrap();
            }
            fout.write(name.as_bytes()).unwrap();

            if quantize {
                print!("quantizing .. ");
                work.resize(nelements as usize, 0.0);

                let curr_size;
                let mut hist_cur = vec![0; 16];

                match otype {
                    TYPE_Q4_0 => {
                        curr_size = quantize_q4_0(
                            &mut data_f32,
                            &mut work,
                            nelements,
                            ne[0],
                            qk as i32,
                            &mut hist_cur,
                        )
                    }
                    TYPE_Q4_1 => {
                        curr_size = quantize_q4_1(
                            &mut data_f32,
                            &mut work,
                            nelements,
                            ne[0],
                            qk as i32,
                            &mut hist_cur,
                        )
                    }
                    _ => {
                        println!("Unsupported type");
                        return Ok(false);
                    }
                }

                // We divide curr size by 4
                for i in 0..curr_size / 4 {
                    fout.write(&work[i].to_le_bytes()).unwrap();
                }

                total_size_new += curr_size;

                print!(
                    "size = {:>8.2} MB -> {:>8.2} MB | hist: ",
                    nelements as f32 * 4.0 / 1024.0 / 1024.0,
                    curr_size as f32 / 1024.0 / 1024.0
                );

                for (i, val) in hist_cur.iter().enumerate() {
                    hist_all[i] += val;
                    print!("{:>5.3} ", *val as f32 / nelements as f32);
                }
                println!();
            } else {
                fout.write(&data_u8).unwrap();
                println!("size = {:>8.3} MB", data_u8.len() as f64 / 1024.0 / 1024.0);
                total_size_new += data_u8.len();
            }

            total_size_org += (nelements * 4) as usize;
        }

        println!(
            "model size: {:>8.2}",
            total_size_org as f32 / 1024.0 / 1024.0
        );

        println!(
            "quant size: {:>8.2}",
            total_size_new as f32 / 1024.0 / 1024.0
        );

        {
            let sum_all: i64 = hist_all.iter().sum();

            print!("hist: ");
            for hist in hist_all {
                print!("{:>5.3} ", hist as f32 / sum_all as f32);
            }
            println!();
        }
    }

    return Ok(true);
}
