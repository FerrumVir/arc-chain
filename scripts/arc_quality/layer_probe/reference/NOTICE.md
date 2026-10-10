This directory retains the official Moonshot Kimi-K2.6 configuration and
`modeling_deepseek.py`, revision `7eb5002f6aadc958aed6a9177b7ed26bb94011bb`,
unchanged. Copyright/license notices in that source are retained; Apache-2.0
license text is included. `provenance.json` is the historical ARC-66 provenance
record: its formula-vector description describes that earlier work, **not** this
layer execution. Current execution provenance is in each generated reference.json.

`official.py` checks both SHA-256 pins and compiles the original AST definitions
listed in `NAMES` without rewriting any method body. It provides torch, typing,
math, a SimpleNamespace configuration, the SiLU activation and eager attention
class mapping that their original import context supplies. This executes the
actual dense/MoE decoder forward, native gate/expert selection, attention, YaRN, RMSNorm and MLP. The driver uses
PyTorch embedding and final linear projection, matching the reference model's
original embedding -> layer -> final norm -> head graph. It deliberately excludes
remote loaders, FlashAttention, Transformers wrappers, distributed expert parallelism and cache execution.
The one-, two- and three-layer graphs and fixture dimensions are experimental, not full K2.6.
