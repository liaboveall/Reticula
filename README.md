# Reticula

A local-first, real-time collaborative spreadsheet for the browser. The calculation engine is written in Rust and compiled to WebAssembly.

## Status

Early development. There is nothing to try yet: this repository currently holds only the project scaffolding.

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
