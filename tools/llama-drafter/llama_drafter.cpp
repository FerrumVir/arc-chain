// llama.cpp drafter for ARC's exact verifier (crates/arc-inference/src/draft_verify.rs).
//
// Speaks ProcessDrafter's line protocol on stdin/stdout:
//   start:   prints "READY <n_vocab>"
//   request: "PROPOSE <max_draft> <min_margin> <n> <stream...> <m> <generated...>"
//            stream = every token the verifier consumed, then the pending token
//   reply:   "OK <micros> <count> <tokens...> <margins...>" or "ERR <reason>"
//            margin = first choice's logit minus the second's, after the penalty
//   "QUIT" exits.
//
// It keeps one KV cache, reuses its longest common prefix with each new
// stream and drops the rest, then proposes greedily from the stream's last
// position. Each choice applies ARC's own repetition penalty first (every
// occurrence among the last 64 generated or drafted tokens: positive logits
// x5/6, others x6/5) and takes the first maximum, as ARC's worker does. It
// stops after the first proposal whose margin is below min_margin, so no time
// is spent drafting past a near-tie the verifier will decide itself. The
// verifier checks every proposal exactly, so nothing here affects ARC's
// output, only how many drafts it accepts.
//
// usage: llama-drafter MODEL.gguf [n_ctx]

#include "llama.h"

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdio>
#include <iostream>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

namespace {

void quiet_log(enum ggml_log_level level, const char * text, void * /*user*/) {
    if (level >= GGML_LOG_LEVEL_WARN) {
        std::fputs(text, stderr);
    }
}

int argmax_first(const std::vector<float> & v) {
    int best = 0;
    for (int i = 1; i < (int) v.size(); ++i) {
        if (v[i] > v[best]) {
            best = i;
        }
    }
    return best;
}

// ARC's select_next_token_with_repetition_penalty, on float logits.
void arc_penalty(std::vector<float> & logits, const std::vector<llama_token> & history) {
    const size_t n = history.size();
    const size_t stop = n > 64 ? n - 64 : 0;
    for (size_t j = n; j > stop; --j) {
        const llama_token t = history[j - 1];
        if (t < 0 || t >= (llama_token) logits.size()) {
            continue;
        }
        float & x = logits[t];
        x = x > 0.0f ? x * 5.0f / 6.0f : x * 6.0f / 5.0f;
    }
}

struct Drafter {
    llama_model *         model = nullptr;
    llama_context *       ctx   = nullptr;
    const llama_vocab *   vocab = nullptr;
    llama_batch           batch{};
    int                   n_vocab = 0;
    int                   n_batch = 512;
    int                   n_ctx   = 0;
    std::vector<llama_token> cached;  // tokens whose K/V the context holds

    // Decodes tokens[from..] after `cached`, keeping logits for the last one.
    bool feed(const std::vector<llama_token> & tokens, size_t from) {
        for (size_t start = from; start < tokens.size(); start += (size_t) n_batch) {
            const size_t end = std::min(tokens.size(), start + (size_t) n_batch);
            batch.n_tokens = 0;
            for (size_t j = start; j < end; ++j) {
                const int b = batch.n_tokens++;
                batch.token[b] = tokens[j];
                batch.pos[b] = (llama_pos) j;
                batch.n_seq_id[b] = 1;
                batch.seq_id[b][0] = 0;
                batch.logits[b] = j + 1 == tokens.size() ? 1 : 0;
            }
            if (llama_decode(ctx, batch) != 0) {
                return false;
            }
        }
        return true;
    }

    std::string propose(const std::vector<llama_token> & stream, const std::vector<llama_token> & generated,
                        size_t max_draft, float min_margin) {
        if (stream.empty()) {
            return "ERR empty stream";
        }
        if ((int) (stream.size() + max_draft) > n_ctx) {
            return "OK 0 0";
        }
        // Reuse the longest common prefix; the last stream token is always
        // forwarded again so its logits are fresh.
        size_t common = 0;
        while (common < cached.size() && common < stream.size() && cached[common] == stream[common]) {
            ++common;
        }
        common = std::min(common, stream.size() - 1);
        if (!llama_memory_seq_rm(llama_get_memory(ctx), 0, (llama_pos) common, -1)) {
            llama_memory_clear(llama_get_memory(ctx), true);
            common = 0;
        }
        cached.assign(stream.begin(), stream.begin() + (long) common);
        if (!feed(stream, common)) {
            cached.clear();
            llama_memory_clear(llama_get_memory(ctx), true);
            return "ERR decode failed";
        }
        cached = stream;

        std::vector<llama_token> history = generated;
        std::vector<llama_token> drafts;
        std::vector<float> margins;
        std::vector<float> logits((size_t) n_vocab);
        for (size_t i = 0; i < max_draft; ++i) {
            const float * row = llama_get_logits_ith(ctx, -1);
            if (row == nullptr) {
                break;
            }
            logits.assign(row, row + n_vocab);
            arc_penalty(logits, history);
            const llama_token next = (llama_token) argmax_first(logits);
            float second = -INFINITY;
            for (int t = 0; t < n_vocab; ++t) {
                if (t != next && logits[(size_t) t] > second) {
                    second = logits[(size_t) t];
                }
            }
            const float margin = logits[(size_t) next] - second;
            drafts.push_back(next);
            margins.push_back(margin);
            history.push_back(next);
            if (llama_vocab_is_eog(vocab, next) || i + 1 == max_draft || margin < min_margin) {
                break;
            }
            std::vector<llama_token> one = cached;
            one.push_back(next);
            if (!feed(one, cached.size())) {
                break;
            }
            cached.push_back(next);
        }
        std::ostringstream out;
        out << drafts.size();
        for (llama_token t : drafts) {
            out << ' ' << t;
        }
        char buf[32];
        for (float m : margins) {
            std::snprintf(buf, sizeof(buf), " %.4f", m);
            out << buf;
        }
        return out.str();
    }
};

bool read_tokens(std::istringstream & in, std::vector<llama_token> & out) {
    long n = -1;
    if (!(in >> n) || n < 0) {
        return false;
    }
    out.resize((size_t) n);
    for (auto & t : out) {
        if (!(in >> t)) {
            return false;
        }
    }
    return true;
}

}  // namespace

int main(int argc, char ** argv) {
    if (argc < 2) {
        std::printf("ERR usage: %s MODEL.gguf [n_ctx]\n", argv[0]);
        return 2;
    }
    llama_log_set(quiet_log, nullptr);
    llama_backend_init();
    Drafter d;
    llama_model_params mparams = llama_model_default_params();
    mparams.n_gpu_layers = 0;
    d.model = llama_model_load_from_file(argv[1], mparams);
    if (d.model == nullptr) {
        std::printf("ERR cannot load %s\n", argv[1]);
        return 1;
    }
    d.vocab = llama_model_get_vocab(d.model);
    d.n_vocab = llama_vocab_n_tokens(d.vocab);
    const unsigned threads = std::max(1u, std::thread::hardware_concurrency());
    llama_context_params cparams = llama_context_default_params();
    cparams.n_ctx = argc > 2 ? (uint32_t) std::stoul(argv[2]) : 4096;
    cparams.n_batch = (uint32_t) d.n_batch;
    cparams.n_ubatch = (uint32_t) d.n_batch;
    cparams.n_seq_max = 1;
    cparams.n_threads = (int32_t) threads;
    cparams.n_threads_batch = (int32_t) threads;
    cparams.no_perf = true;
    d.ctx = llama_init_from_model(d.model, cparams);
    if (d.ctx == nullptr) {
        std::printf("ERR cannot create a context\n");
        return 1;
    }
    d.n_ctx = (int) llama_n_ctx(d.ctx);
    d.batch = llama_batch_init(d.n_batch, 0, 1);
    std::fprintf(stderr, "llama-drafter: n_vocab %d, n_ctx %d, %u threads\n", d.n_vocab, d.n_ctx, threads);
    std::printf("READY %d\n", d.n_vocab);
    std::fflush(stdout);

    std::string line;
    while (std::getline(std::cin, line)) {
        std::istringstream in(line);
        std::string cmd;
        in >> cmd;
        if (cmd == "QUIT") {
            break;
        }
        if (cmd != "PROPOSE") {
            std::printf("ERR unknown request\n");
            std::fflush(stdout);
            continue;
        }
        long max_draft = -1;
        float min_margin = 0.0f;
        std::vector<llama_token> stream, generated;
        if (!(in >> max_draft >> min_margin) || max_draft < 0 || !read_tokens(in, stream) ||
            !read_tokens(in, generated)) {
            std::printf("ERR malformed request\n");
            std::fflush(stdout);
            continue;
        }
        const auto started = std::chrono::steady_clock::now();
        const std::string reply = d.propose(stream, generated, (size_t) max_draft, min_margin);
        const long long micros = std::chrono::duration_cast<std::chrono::microseconds>(
                                     std::chrono::steady_clock::now() - started)
                                     .count();
        if (reply.rfind("ERR", 0) == 0 || reply.rfind("OK", 0) == 0) {
            std::printf("%s\n", reply.c_str());
        } else {
            std::printf("OK %lld %s\n", micros, reply.c_str());
        }
        std::fflush(stdout);
    }
    llama_batch_free(d.batch);
    llama_free(d.ctx);
    llama_model_free(d.model);
    llama_backend_free();
    return 0;
}
