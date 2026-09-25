# RandomX, Boltchain variant ("RandomBOLT")

Source: tevador/RandomX as bundled in the `randomx-rs` 1.6.0 crate (BSD-3-Clause, see LICENSE).

The only change is `RANDOMX_ARGON_SALT` in `src/configuration.h`:

    "RandomX\x03"  ->  "RandomBOLT\x01"

Changing the Argon2 salt is the customization RandomX's documentation recommends for projects
other than Monero. Every other parameter (cache and dataset size, program size, iterations) is
unchanged, so the performance profile and the audits of RandomX carry over, while hashes are
incompatible with Monero's: Monero hashrate and "rx/0" hashrate markets cannot be pointed at
Boltchain (ADR 0007 §1).

`randomx-rs` builds this directory because `.cargo/config.toml` sets `RANDOMX_DIR`.
