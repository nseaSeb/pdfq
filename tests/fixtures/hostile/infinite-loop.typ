// Effectively infinite: 10^10 iterations. `while true` is rejected statically by Typst.
#for i in range(100000) { for j in range(100000) { } }
