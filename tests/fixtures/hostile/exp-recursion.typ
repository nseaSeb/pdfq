// Distinct arguments defeat Typst's memoization: 2^40 calls.
#let f(n, k) = if n == 0 { k } else { f(n - 1, 2 * k) + f(n - 1, 2 * k + 1) }
#f(40, 0)
