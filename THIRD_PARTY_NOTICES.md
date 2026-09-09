# Native encoding components

FileTwin's developer setup downloads artifacts only when explicitly requested.
Keep these notices and the downloaded artifact licenses when distributing a
native bundle. Rust dependency licenses are recorded by each dependency crate.

- SSCD model: Meta's published `sscd_disc_mixup.torchscript.pt`, SHA-256
  `9f26bd4c848cc19b73d2ae92eea6e04886f61a7b764ceb7a13aeee62e6a6db56`.
  [Official project and model documentation](https://github.com/facebookresearch/sscd-copy-detection).
  [Source code license (MIT)](https://github.com/facebookresearch/sscd-copy-detection/blob/main/LICENSE).
  FileTwin converts the public model to ONNX; it does not train or redistribute a
  different model. No calibration claims are inherited from SSCD's benchmarks.
- ONNX Runtime 1.28.2: [Microsoft's release](https://github.com/microsoft/onnxruntime/releases/tag/v1.28.2),
  MIT license and upstream third-party notices, preserved by setup-native.py.
- PDFium Chromium 8044: [binary publisher](https://github.com/bblanchon/pdfium-binaries/releases/tag/chromium%2F8044),
  BSD-style PDFium license and bundled third-party licenses, preserved by setup.
- FFmpeg: separately installed executable, [license/build information](https://ffmpeg.org/legal.html).
  Its applicable LGPL/GPL terms depend on the selected build; FileTwin does not
  bundle FFmpeg in this developer source tree. The media worker invokes its CLI.

PyTorch, ONNX and Python are conversion/validation tools only. They are not part
of FileTwin's production runtime. Native release packaging and redistribution
qualification remain separate from this developer implementation.
