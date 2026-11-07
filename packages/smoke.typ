// Build-time smoke test: uses every package enabled in packages/packages.txt.
// Uncomment the example of each package you enable there.

#import "@preview/tiaoma:0.3.0"
#tiaoma.qrcode("https://example.com")

#import "@preview/zero:0.7.1": num
#num("1234.5")

// #import "@preview/cetz:0.5.3"
// #cetz.canvas(cetz.draw.circle((0, 0)))

// #import "@preview/oxifmt:1.0.0": strfmt
// #strfmt("{}", 1)

// #import "@preview/showybox:2.0.4": showybox
// #showybox[Box]

// #import "@preview/codly:2.0.0" as codly
// #show raw.where(block: true): codly.new
// ```rust
// fn main() {}
// ```
