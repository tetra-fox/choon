default:
    @just --list

# cargo nextest with the default features, then the doc examples nextest does not run
test:
    cargo nextest run
    cargo test --doc

# formatting, clippy on the library and program with their tests, on the library alone without
# the program, with `serde`, with `testing` and for WebAssembly, and the docs as docs.rs builds
# them
lint:
    cargo fmt --all --check
    cargo fmt --manifest-path fuzz/Cargo.toml --all --check
    cargo fmt --manifest-path corpus/reader/Cargo.toml --all --check
    cargo fmt --manifest-path web/Cargo.toml --all --check
    cargo clippy --all-targets -- -D warnings
    cargo clippy --no-default-features -- -D warnings
    cargo clippy --no-default-features --features serde -- -D warnings
    cargo clippy --no-default-features --features testing -- -D warnings
    cargo clippy --no-default-features --features serde --target wasm32-unknown-unknown -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --no-default-features --features serde

# the sample files of samples/README.md, written again by the encoders' own programs
samples:
    nix develop .#samples -c samples/make.sh

# the files of torture/README.md, written again by the formats' own programs; `just torture long`
# adds the hours-long ones
torture *ARGS:
    nix develop .#torture -c torture/make.sh {{ ARGS }}

# every torture case through a release build, judged against its expectation
torture-run RESULTS="target/torture.tsv":
    cargo build --release
    nix develop .#torture -c torture/run.py {{ RESULTS }} target/release/choon

# a corpus of files with a known history, made from lossless masters and read: corpus/README.md
corpus *ARGS:
    nix develop .#corpus -c corpus/corpus {{ ARGS }}

# the fuzz recipes need nightly cargo and cargo-fuzz: run them inside `nix develop .#fuzz`

# copy the sample files into fuzz/corpus/<target>, each in full and sampled, with the measures
# and thresholds the example makes, and the torture set's seeds where `just torture` wrote them
fuzz-seed: (lock "fuzz")
    #!/usr/bin/env bash
    set -euo pipefail
    cd fuzz
    for target in $(cargo fuzz list); do mkdir -p "corpus/$target"; done
    # a seed of the targets that read a file: a byte for the coverage, then the file
    seed() {
      for coverage in 0 1; do
        { printf "\\$coverage"; cat "$2"; } > "corpus/$1/$coverage-$(basename "$2")"
      done
    }
    for file in ../src/fixtures/*.{m4a,aac,wv,ape} ../torture/seeds/*; do
      [ -e "$file" ] || continue
      seed any_bytes "$file"
      case $file in
        *.wv) seed wavpack "$file" ;;
        *.ape) seed ape "$file" ;;
        *.dsf | *.dff) seed dsd "$file" ;;
        *he-aac* | *.aac | *aac*.m4a) seed aac "$file" ;;
        *opus*) seed opus "$file" ;;
      esac
    done
    head -c 16384 ../src/fixtures/clip.pcm > corpus/samples/clip
    cargo run --quiet --example seeds -- corpus

# the crate in DIR is a workspace of its own, so its lockfile starts as a copy of the main one:
# it builds against the versions the crate ships with, and cargo adds only its own crates
[private]
lock DIR:
    cp Cargo.lock {{ DIR }}/Cargo.lock

# run one fuzz target, indefinitely unless SECONDS is given. an input that takes a minute is a hang
fuzz TARGET SECONDS="": fuzz-seed
    #!/usr/bin/env bash
    set -euo pipefail
    dict=()
    [ -e "fuzz/dict/{{ TARGET }}.dict" ] && dict=("-dict=fuzz/dict/{{ TARGET }}.dict")
    # under the address sanitizer and compare tracing one grid scan takes minutes, and these
    # targets scan every input
    build=()
    case {{ TARGET }} in samples | thresholds_json) build=(--sanitizer none --no-trace-compares) ;; esac
    cargo fuzz run "${build[@]}" {{ TARGET }} -- -timeout=60 -max_len=65536 "${dict[@]}" \
      {{ if SECONDS == "" { "" } else { "-max_total_time=" + SECONDS } }}

# the web recipes need wasm-bindgen's program, wasm-opt and deno: run them inside
# `nix develop .#web`

# the analyzer as WebAssembly with SIMD, the JavaScript wasm-bindgen writes for it, and the page
# of web/page copied into web/dist with the commit it is built from: web/README.md. wasm-opt
# makes the wasm about a quarter smaller and no faster
web: (lock "web")
    #!/usr/bin/env bash
    set -euo pipefail
    RUSTFLAGS="-C target-feature=+simd128" cargo build --manifest-path web/Cargo.toml --lib --release --target wasm32-unknown-unknown
    # without the default path the JavaScript names no wasm file, as web/dist has the wasm as
    # base64 alone
    wasm-bindgen --target web --omit-default-module-path --out-dir web/page/wasm web/target/wasm32-unknown-unknown/release/choon_web.wasm
    wasm-opt -O2 web/page/wasm/choon_web_bg.wasm -o web/page/wasm/choon_web_bg.wasm
    # the wasm as a module the worker imports
    { printf 'export default "'; base64 < web/page/wasm/choon_web_bg.wasm | tr -d '\n'; printf '";\n'; } > web/page/wasm/choon_web_wasm.js
    rm -rf web/dist
    mkdir -p web/dist/wasm
    cp web/page/page.css web/page/page.js web/page/worker.js web/dist/
    cp web/page/wasm/choon_web.js web/page/wasm/choon_web_wasm.js web/dist/wasm/
    # the page names the commit it is built from, and says when the checkout differs from it
    changed=""
    git diff --quiet HEAD || changed=", with changes not committed"
    sed -e "s|@REPOSITORY@|$(sed -n 's/^repository = "\(.*\)"/\1/p' Cargo.toml)|g" \
      -e "s|@COMMIT@|$(git rev-parse HEAD)|g" \
      -e "s|@COMMIT_SHORT@|$(git rev-parse --short HEAD)|g" \
      -e "s|@CHANGED@|$changed|g" \
      web/page/index.html > web/dist/index.html

# the types of the page's JavaScript, from its JSDoc
web-check: web
    deno check --config web/page/deno.json web/page/page.js web/page/worker.js

# web/dist served by Cloudflare as the Worker of web/wrangler.jsonc. wrangler reads its token
# from CLOUDFLARE_API_TOKEN, or asks to log in
web-deploy: web-check
    wrangler deploy --config web/wrangler.jsonc

# the library alone and the program on the manifest's rust-version
msrv:
    CARGO_TARGET_DIR=target/msrv nix develop .#msrv -c cargo build --locked --no-default-features
    CARGO_TARGET_DIR=target/msrv nix develop .#msrv -c cargo build --locked

# the gate for every push; ci.yml runs it
ci: lint test msrv

# package and build the crate as crates.io would, without uploading; release.yml runs it
publish-check:
    cargo publish --dry-run
