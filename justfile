set shell := ["bash", "-uc"]

_default:
    @just --list

# Format.
fmt:
    cargo fmt

# Same, reporting instead of writing. Used by `just check`.
fmt-check:
    cargo fmt --check

# `-D warnings` so a new warning fails the gate, with the three that predate
# the justfile allowed by name rather than silenced wholesale: clearing one
# means deleting its `-A`, and the gate starts catching it from then on.
lint:
    cargo clippy --all-targets -- -D warnings \
        -A dead_code -A clippy::collapsible_if -A clippy::while_let_loop

build:
    cargo build

test:
    cargo test

# Install claux into ~/.cargo/bin; the whole install story.
install:
    cargo install --path . --force

# The full gate before pushing.
check: fmt-check lint build test
