# PDFQ

Compile [Typst](https://typst.app) documents to PDF over HTTP — sync or async, stateless, safe against untrusted documents.

> Work in progress. See `docs/api-spec.md` for the planned API.

## Development

```sh
cargo test
cargo run -p pdfq-cli -- compile examples/invoice/main.typ --data examples/invoice/data.json -o invoice.pdf
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
