# Model quality gate (2026-09-19)

Current bounded quality subgates:

- Tokenizer vectors: **11/11** exact against the pinned llama.cpp tokenizer vectors.
- Prompt suite: **6/6** semantic prompts passed bounded content checks.
- Direct sampled greedy IDs: **5/6** exact; Paris shares the first eight content IDs and then differs at terminal EOG (llama.cpp emits ID `2`, ARC continues to the 12-token bound).
- Paris content: first eight content IDs and decoded sentence match before the terminal difference.

The evidence is a same-GGUF local diagnostic using the pinned Llama-2-7B Q4_K_M artifact. A bounded two-machine forward query matched exact logits for 3/3 inputs, but partitioned execution was 34.05× slower than the local oracle (157,175 ms vs 4,616 ms). This does not establish broad quality, production tokenizer qualification, API parity, cost, or commercial readiness; arbitrary WAN conditions cannot guarantee speedup. No live v3 validator restart occurred.
