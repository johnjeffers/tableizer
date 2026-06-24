# Tableizer dev commands — run `just` to list, `just <recipe>` to run.
# Requires `just` (cargo install just); `dev` also needs `cargo-watch`.

# List available recipes
default:
    @just --list

# Open the desktop app (release build). Pass a file to open it directly, or omit to start empty.
run file="":
    if [ -n "{{file}}" ]; then cargo run --release -p tableizer -- "{{file}}"; else cargo run --release -p tableizer; fi

# UI dev loop: auto-rebuild + re-run on save (debug build = fastest rebuilds). Optionally pass a file.
dev file="":
    if [ -n "{{file}}" ]; then cargo watch -x "run -p tableizer -- '{{file}}'"; else cargo watch -x "run -p tableizer"; fi

# Run all tests
test:
    cargo test --workspace

# Run one engine test by name (fast inner loop), e.g. `just test-one offset`
test-one name:
    cargo test -p tableizer-core {{name}}

# Format the whole workspace
fmt:
    cargo fmt --all

# Lint with warnings-as-errors
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Full pre-commit gate (mirrors CI): format check + lint + tests
ci:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

# Generate a synthetic CSV, e.g. `just gen /tmp/big.csv 5000000`
gen file rows:
    cargo run --release -p tableizer-core --example gen_csv -- "{{file}}" {{rows}}

# Time the load path on a file (perf harness)
bench file:
    cargo run --release -p tableizer-core --example bench_load -- "{{file}}"

# License + advisory audit (needs cargo-deny)
deny:
    cargo deny check

# Build Tableizer.app into dist/ (macOS only)
build:
    bash scripts/package-macos.sh

# Build the macOS .app bundle + .dmg into dist/ (macOS only)
package-mac:
    bash scripts/package-macos.sh dmg

# Build and install Tableizer.app into /Applications (macOS only)
install: build
    rm -rf /Applications/Tableizer.app
    ditto dist/Tableizer.app /Applications/Tableizer.app
    @echo "installed → /Applications/Tableizer.app"

# Remove Tableizer.app from /Applications (macOS only)
uninstall:
    rm -rf /Applications/Tableizer.app
    @echo "removed /Applications/Tableizer.app"
