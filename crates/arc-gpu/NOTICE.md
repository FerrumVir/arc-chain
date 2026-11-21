# Notices

`src/matmul.metal` adapts the tiled matrix-multiply pattern of
`ggml-metal.metal` from ggml (llama.cpp, MIT license) to integer-only
per-row INT8 arithmetic: four simdgroups per threadgroup, one output row per
simdgroup, 32 threads splitting the inner dimension with a `simd_sum`
reduction, and `char4` vector loads.
