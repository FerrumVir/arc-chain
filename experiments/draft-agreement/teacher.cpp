// Draft-agreement experiment, llama.cpp side (scratch branch, not for merge).
//
// Teacher-forces a llama.cpp model on the exact tokens ARC's engine forwarded
// and reports, at every generated position, llama.cpp's argmax given ARC's
// prefix. One pass per sequence, logits for every generated position, CPU,
// greedy (no sampler, no temperature).
//
// Input, one sequence per line (whitespace separated):
//   id first n_targets n_fed fed[0..n_fed] targets[0..n_targets]
// where the logits after fed[first + i] must predict targets[i].
// Output, one JSON object per line:
//   raw[i]  argmax of llama.cpp's logits
//   pen[i]  argmax after ARC's repetition penalty (every occurrence of the
//           last 64 targets before i: positive logits x5/6, others x6/5)
//   raw_gap[i], pen_gap[i]  llama.cpp logit of its choice minus that of ARC's
//           token (0 when they agree)
//   raw_rank[i], pen_rank[i]  tokens llama.cpp prefers to ARC's (0 = agree,
//           1 = ARC's token is its second choice)
//   raw_margin[i], pen_margin[i]  its first choice minus its second
//
// usage: teacher MODEL.gguf INPUT.txt OUTPUT.jsonl

#include "llama.h"

#include <algorithm>
#include <cmath>
#include <chrono>
#include <cstdio>
#include <fstream>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

struct Sequence {
    std::string id;
    long first = 0;
    std::vector<llama_token> fed;
    std::vector<llama_token> targets;
};

static int argmax_first(const std::vector<float> & v) {
    int best = 0;
    for (int i = 1; i < (int) v.size(); ++i) {
        if (v[i] > v[best]) {
            best = i;
        }
    }
    return best;
}

static void arc_penalty(std::vector<float> & logits, const std::vector<llama_token> & targets, long upto) {
    const long stop = upto > 64 ? upto - 64 : 0;
    for (long j = upto - 1; j >= stop; --j) {
        const llama_token t = targets[j];
        if (t < 0 || t >= (llama_token) logits.size()) {
            continue;
        }
        float & x = logits[t];
        x = x > 0.0f ? x * 5.0f / 6.0f : x * 6.0f / 5.0f;
    }
}

// Number of tokens llama.cpp prefers to `want` (0 = its first choice), in the
// first-maximum order ARC's argmax uses, and its first-minus-second margin.
static void rank_and_margin(const std::vector<float> & v, int want, int best, int & rank, float & margin) {
    const float target = v[want];
    rank = 0;
    float second = -INFINITY;
    for (int i = 0; i < (int) v.size(); ++i) {
        if (v[i] > target || (v[i] == target && i < want)) {
            ++rank;
        }
        if (i != best && v[i] > second) {
            second = v[i];
        }
    }
    margin = v[best] - second;
}

static void write_ints(std::ofstream & out, const char * key, const std::vector<int> & v) {
    out << ",\"" << key << "\":[";
    for (size_t i = 0; i < v.size(); ++i) {
        out << (i ? "," : "") << v[i];
    }
    out << "]";
}

static void write_floats(std::ofstream & out, const char * key, const std::vector<float> & v) {
    out << ",\"" << key << "\":[";
    char buf[32];
    for (size_t i = 0; i < v.size(); ++i) {
        std::snprintf(buf, sizeof(buf), "%.4f", v[i]);
        out << (i ? "," : "") << buf;
    }
    out << "]";
}

int main(int argc, char ** argv) {
    if (argc != 4) {
        std::fprintf(stderr, "usage: %s MODEL.gguf INPUT.txt OUTPUT.jsonl\n", argv[0]);
        return 2;
    }
    std::vector<Sequence> seqs;
    long max_fed = 0;
    {
        std::ifstream in(argv[2]);
        std::string line;
        while (std::getline(in, line)) {
            if (line.empty()) {
                continue;
            }
            std::istringstream ss(line);
            Sequence s;
            long n_targets = 0, n_fed = 0;
            ss >> s.id >> s.first >> n_targets >> n_fed;
            if (!ss || n_targets <= 0 || n_fed <= 0 || s.first + n_targets != n_fed) {
                std::fprintf(stderr, "bad header in line: %.80s\n", line.c_str());
                return 2;
            }
            s.fed.resize(n_fed);
            s.targets.resize(n_targets);
            for (auto & t : s.fed) {
                ss >> t;
            }
            for (auto & t : s.targets) {
                ss >> t;
            }
            if (!ss) {
                std::fprintf(stderr, "%s: truncated line\n", s.id.c_str());
                return 2;
            }
            // Teacher forcing feeds ARC's own tokens: after the first target,
            // every fed token is the previous target.
            for (long i = 0; i + 1 < n_targets; ++i) {
                if (s.fed[s.first + 1 + i] != s.targets[i]) {
                    std::fprintf(stderr, "%s: fed[%ld] is not target %ld\n", s.id.c_str(), s.first + 1 + i, i);
                    return 2;
                }
            }
            max_fed = std::max(max_fed, n_fed);
            seqs.push_back(std::move(s));
        }
    }
    if (seqs.empty()) {
        std::fprintf(stderr, "no sequences in %s\n", argv[2]);
        return 2;
    }

    llama_backend_init();
    llama_model_params mparams = llama_model_default_params();
    mparams.n_gpu_layers = 0;
    llama_model * model = llama_model_load_from_file(argv[1], mparams);
    if (model == nullptr) {
        std::fprintf(stderr, "failed to load %s\n", argv[1]);
        return 1;
    }
    const llama_vocab * vocab = llama_model_get_vocab(model);
    const int n_vocab = llama_vocab_n_tokens(vocab);

    const int n_batch = 512;
    const unsigned threads = std::max(1u, std::thread::hardware_concurrency());
    llama_context_params cparams = llama_context_default_params();
    cparams.n_ctx = (uint32_t) (((max_fed + 255) / 256) * 256);
    cparams.n_batch = n_batch;
    cparams.n_ubatch = n_batch;
    cparams.n_seq_max = 1;
    cparams.n_threads = (int32_t) threads;
    cparams.n_threads_batch = (int32_t) threads;
    cparams.no_perf = true;
    llama_context * ctx = llama_init_from_model(model, cparams);
    if (ctx == nullptr) {
        std::fprintf(stderr, "failed to create a context\n");
        return 1;
    }
    std::fprintf(stderr, "teacher: %zu sequences, n_vocab %d, n_ctx %u, %u threads\n", seqs.size(), n_vocab,
                 llama_n_ctx(ctx), threads);

    llama_batch batch = llama_batch_init(n_batch, 0, 1);
    std::ofstream out(argv[3]);
    std::vector<float> logits(n_vocab);
    for (const Sequence & s : seqs) {
        const auto started = std::chrono::steady_clock::now();
        const long n_fed = (long) s.fed.size();
        const long n_targets = (long) s.targets.size();
        std::vector<int> raw(n_targets), pen(n_targets), raw_rank(n_targets), pen_rank(n_targets);
        std::vector<float> raw_gap(n_targets), pen_gap(n_targets), raw_margin(n_targets), pen_margin(n_targets);
        llama_memory_clear(llama_get_memory(ctx), true);
        for (long start = 0; start < n_fed; start += n_batch) {
            const long end = std::min(n_fed, start + (long) n_batch);
            batch.n_tokens = 0;
            for (long j = start; j < end; ++j) {
                const int b = batch.n_tokens;
                batch.token[b] = s.fed[j];
                batch.pos[b] = (llama_pos) j;
                batch.n_seq_id[b] = 1;
                batch.seq_id[b][0] = 0;
                batch.logits[b] = j >= s.first ? 1 : 0;
                batch.n_tokens++;
            }
            if (llama_decode(ctx, batch) != 0) {
                std::fprintf(stderr, "%s: llama_decode failed at %ld\n", s.id.c_str(), start);
                return 1;
            }
            for (long j = std::max(start, s.first); j < end; ++j) {
                const float * row = llama_get_logits_ith(ctx, (int32_t) (j - start));
                if (row == nullptr) {
                    std::fprintf(stderr, "%s: no logits at %ld\n", s.id.c_str(), j);
                    return 1;
                }
                const long i = j - s.first;
                const llama_token want = s.targets[i];
                if (want < 0 || want >= n_vocab) {
                    std::fprintf(stderr, "%s: target %d outside the vocabulary\n", s.id.c_str(), want);
                    return 1;
                }
                logits.assign(row, row + n_vocab);
                raw[i] = argmax_first(logits);
                raw_gap[i] = logits[raw[i]] - logits[want];
                rank_and_margin(logits, want, raw[i], raw_rank[i], raw_margin[i]);
                arc_penalty(logits, s.targets, i);
                pen[i] = argmax_first(logits);
                pen_gap[i] = logits[pen[i]] - logits[want];
                rank_and_margin(logits, want, pen[i], pen_rank[i], pen_margin[i]);
            }
        }
        const double seconds =
            std::chrono::duration<double>(std::chrono::steady_clock::now() - started).count();
        out << "{\"id\":\"" << s.id << "\",\"n_fed\":" << n_fed << ",\"seconds\":" << seconds;
        write_ints(out, "raw", raw);
        write_ints(out, "pen", pen);
        write_floats(out, "raw_gap", raw_gap);
        write_floats(out, "pen_gap", pen_gap);
        write_ints(out, "raw_rank", raw_rank);
        write_ints(out, "pen_rank", pen_rank);
        write_floats(out, "raw_margin", raw_margin);
        write_floats(out, "pen_margin", pen_margin);
        out << "}\n";
        out.flush();
        long agree = 0;
        for (long i = 0; i < n_targets; ++i) {
            agree += pen[i] == s.targets[i];
        }
        std::fprintf(stderr, "%s: %ld fed, %ld targets, %ld agree (penalty mirrored), %.1f s\n", s.id.c_str(), n_fed,
                     n_targets, agree, seconds);
    }
    llama_batch_free(batch);
    llama_free(ctx);
    llama_model_free(model);
    llama_backend_free();
    return 0;
}
