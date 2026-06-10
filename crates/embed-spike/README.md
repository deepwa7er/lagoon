# embed-spike — Phase 3 pre-phase spike

Throwaway crate (throwaway branch `spike/candle-embeddings`) answering the
buildout plan's Phase 3 gate question: **can candle run all-MiniLM-L6-v2
fast enough on iPhone-class hardware** (<100ms per embedding)?

## Running it

Model files are not committed. Fetch them once:

```sh
mkdir -p crates/embed-spike/model
for f in model.safetensors tokenizer.json config.json; do
  curl -sL -o "crates/embed-spike/model/$f" \
    "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/$f"
done
```

Host run:

```sh
cargo run --release -p buoy-embed-spike
```

iOS simulator run (proves the binary works in an iOS runtime):

```sh
cargo build --release -p buoy-embed-spike --target aarch64-apple-ios-sim
xcrun simctl boot <device-udid>
SIMCTL_CHILD_RAYON_NUM_THREADS=2 xcrun simctl spawn <device-udid> \
  target/aarch64-apple-ios-sim/release/embed-spike \
  "$PWD/crates/embed-spike/model"
```

## Findings (2026-06-09, M-series host, Xcode iPhone 17 simulator)

| Measurement | Result |
|---|---|
| Host (macOS arm64) median embed | ~12.5ms |
| iOS simulator, default rayon threads | ~57ms median, high variance (53–147ms) |
| iOS simulator, rayon capped at 1–2 threads | **~37–38ms median, stable** |
| Model load (mmap safetensors) | 23–76ms |
| `aarch64-apple-ios` (device) target | builds clean |
| Embedding sanity (cosine ordering) | paraphrases 0.48–0.53 vs unrelated 0.05–0.12 — correct |

Decisions these numbers force:

1. **Pure-Rust tokenizer backend is required.** `tokenizers` default
   `onig` backend is a C library that fails to link for `aarch64-apple-ios`
   (undefined `___chkstk_darwin` from the SDK/deployment-target mismatch).
   `default-features = false, features = ["fancy-regex"]` builds clean
   everywhere and MiniLM's WordPiece tokenizer barely uses regex anyway.
2. **Cap candle's rayon pool on mobile (1–2 threads).** Default
   thread-per-core oversubscribes and makes latency worse *and* unstable
   (57ms noisy → 37ms stable in the simulator).
3. Model is 90MB f32 safetensors. Fine for a spike; Phase 3 proper should
   decide between shipping f16 (~45MB) in-bundle vs download-on-first-run.

## What this spike does NOT prove

Simulator numbers come from the Mac's CPU, not an iPhone SoC. A physical
device run is still required before the <100ms criterion is signed off.
The margin (37ms, ~2.7× headroom) and the device target building clean
both point to PASS, but the plan's gate is an iPhone, not a simulator.
