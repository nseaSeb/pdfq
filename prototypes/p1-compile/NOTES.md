# Prototype 1 notes (typst 0.15.1)

- `PagedDocument` lives in `typst-layout`, which `typst` does not re-export: add `typst-layout = "=0.15.1"` as a direct dependency.
- Paths were redesigned: `FileId` = `RootedPath::new(VirtualRoot::Project | Package(spec), VirtualPath::new(..)?).intern()`. `FileId::package()` is deprecated; use `id.get().root()`.
- `World::today` takes `Option<typst::foundations::Duration>`, not `i64`.
- Fonts: `typst_kit::fonts::FontStore` + `embedded()` (feature `embedded-fonts`) gives `book()` and `font(i)` directly.
- `sys.inputs` via `Library::builder().with_inputs(Dict)`; templates read `json(bytes(sys.inputs.data))`.
- Diagnostics: `d.span.id()` + `world.range(d.span)` + `source.lines().byte_to_line_column()` (0-based).
- `..` escaping the root is rejected by Typst itself; the hint mentions the CLI `--root` flag, so the server must filter or rewrite hints.
