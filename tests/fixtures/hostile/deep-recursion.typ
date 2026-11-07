// Unbounded recursion: Typst caps the call depth.
#let f(n) = f(n + 1)
#f(0)
