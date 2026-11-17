# Reticula

A local-first, real-time collaborative spreadsheet for the browser. The calculation engine is written in Rust and compiled to WebAssembly.

## Status

Early development. Nothing is deployed yet. What exists so far is the path from the engine to the screen: the engine runs in a worker, generates a million-row sample from a seed, and sends the cells in view to a canvas grid that scrolls through them. `cargo xtask dev` runs it locally.

## Development

Requires [rustup](https://rustup.rs) (the pinned Rust toolchain installs itself), Node.js 26 and pnpm.

```sh
cargo xtask setup --browsers   # pinned tools into .tools/, dependencies, browsers for end-to-end tests
cargo xtask dev                # development server on http://localhost:5273
cargo xtask ci                 # everything CI runs on a push
```

`cargo xtask` on its own lists the other tasks.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

## Contributing

Bug reports and questions are welcome through issues. Pull requests are not being accepted at this stage.
