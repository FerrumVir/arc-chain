# Data for the modern-model (SmolLM3-3B) checks

`alice_ch1.txt` is Chapter I of Lewis Carroll's *Alice's Adventures in
Wonderland* (1865), a public-domain text. It was extracted from
`https://www.gutenberg.org/cache/epub/11/pg11.txt` (174,311 bytes, SHA-256
`01b38ea4c710a84bc18d0bd41271a5a1a92b94e97b2812f4dece97d4a694725e`, fetched
2026-10-06): the lines from `CHAPTER I.` up to `CHAPTER II.`, with hard-wrapped
paragraphs joined into single lines. No other edits. It is the small public text
for the perplexity comparison against the BF16 reference.

`fr_2026-20493_supplementary.txt` is the "SUPPLEMENTARY INFORMATION" section of
Federal Register document 2026-20493, "Eliminating Obsolete Regulations Related
to the 911 Grant Program" (NTIA and NHTSA final rule, 91 FR 63500, published
2026-10-06). As a work of the U.S. Government it is in the public domain. It was
extracted from
`https://www.federalregister.gov/documents/full_text/text/2026/10/06/2026-20493.txt`
(16,274 bytes, SHA-256
`bd3577655db7c8dd8ce3a3c160ca01dd7c59e3c656fd5b2a9be616a1ff2c00bc`, fetched
2026-10-06): HTML tags removed, entities decoded, wrapped lines joined into
paragraphs. The contact section before it is not included. It was published
after SmolLM3-3B's training data cutoff, so, unlike *Alice*, the model cannot
have memorised it; its perplexity is the more informative of the two.
