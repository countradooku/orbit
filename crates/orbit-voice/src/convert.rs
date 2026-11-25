//! OpenAI's safetensors weights to the GGUF the engine loads: matrices
//! quantized (q8_0 by default), everything else kept in f32.

use std::path::Path;

use candle_core::quantized::{GgmlDType, QTensor, gguf_file};
use candle_core::{DType, Device};

/// Converts `src` (a Hugging Face Whisper `model.safetensors`) to a GGUF at
/// `dest`, quantizing two-dimensional weights to `dtype`. Reports progress
/// in `0.0..=1.0`; stops when `cancelled` returns true.
pub fn safetensors_to_gguf(
    src: &Path,
    dest: &Path,
    dtype: GgmlDType,
    progress: &dyn Fn(f32),
    cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    let fail = |e: candle_core::Error| format!("The model couldn’t be converted: {e}");
    // SAFETY: the file is ours, verified, and not modified while mapped.
    let st = unsafe { candle_core::safetensors::MmapedSafetensors::new(src) }.map_err(fail)?;
    let mut names: Vec<String> = st.tensors().into_iter().map(|(n, _)| n).collect();
    names.sort();
    let total = names.len().max(1) as f32;
    let mut tensors = Vec::with_capacity(names.len());
    for (i, name) in names.into_iter().enumerate() {
        if cancelled() {
            return Err("Cancelled.".to_owned());
        }
        // The output projection is the token embedding, transposed.
        if name == "proj_out.weight" {
            continue;
        }
        let tensor = st.load(&name, &Device::Cpu).map_err(fail)?.to_dtype(DType::F32).map_err(fail)?;
        let quantize = tensor.rank() == 2
            && tensor.dim(1).map_err(fail)? % dtype.block_size() == 0
            && !name.contains("embed_positions");
        let q = QTensor::quantize(&tensor, if quantize { dtype } else { GgmlDType::F32 }).map_err(fail)?;
        tensors.push((name, q));
        progress((i + 1) as f32 / total);
    }
    let refs: Vec<(&str, &QTensor)> = tensors.iter().map(|(n, q)| (n.as_str(), q)).collect();
    let tmp = dest.with_extension("gguf.tmp");
    let write = || -> std::io::Result<()> {
        let mut file = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        gguf_file::write(&mut file, &[], &refs).map_err(std::io::Error::other)?;
        let file = file.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("The converted model couldn’t be saved: {e}"));
    }
    std::fs::rename(&tmp, dest).map_err(|e| format!("The converted model couldn’t be saved: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tests::{TINY, random_weights};
    use crate::model::{Weights, Whisper};
    use candle_core::Tensor;
    use std::collections::HashMap;

    #[test]
    fn converts_hugging_face_weights_to_a_loadable_q8_model() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("model.safetensors");
        // The same tensors as dense f32, as OpenAI publishes them.
        let dense: HashMap<String, Tensor> = random_weights(TINY)
            .into_iter()
            .map(|(n, q)| (n, q.dequantize(&Device::Cpu).unwrap()))
            .chain([("proj_out.weight".to_owned(), Tensor::zeros((300, 64), DType::F32, &Device::Cpu).unwrap())])
            .collect();
        candle_core::safetensors::save(&dense, &src).unwrap();
        let dest = dir.path().join("model.gguf");
        let steps = std::cell::Cell::new(0);
        safetensors_to_gguf(&src, &dest, GgmlDType::Q8_0, &|_| steps.set(steps.get() + 1), &|| false).unwrap();
        assert!(steps.get() > 10);
        assert!(!dest.with_extension("gguf.tmp").exists());
        let weights = Weights::read(&dest, &Device::Cpu).unwrap();
        Whisper::load(weights, TINY).unwrap();
        // Quantized: smaller than the f32 source.
        assert!(std::fs::metadata(&dest).unwrap().len() < std::fs::metadata(&src).unwrap().len() / 2);
        let err = safetensors_to_gguf(&src, &dest, GgmlDType::Q8_0, &|_| {}, &|| true).unwrap_err();
        assert_eq!(err, "Cancelled.");
    }
}
