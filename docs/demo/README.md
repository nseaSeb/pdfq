# Terminal demo

`pdfq-demo.gif` is a short terminal recording of PDFQ (plain compile, template + JSON data, a hostile `#read`, a memory bomb, health check) against the published image `ghcr.io/nseaseb/pdfq:0.1.0-typst0.15.1`.

Regenerate it from the repo root with [vhs](https://github.com/charmbracelet/vhs): `vhs docs/demo/demo.tape`. It needs Docker, curl, jq, poppler (`pdftotext`, `pdfinfo`) and host port 3100 free; the tape starts and stops a `pdfq-demo` container itself.
