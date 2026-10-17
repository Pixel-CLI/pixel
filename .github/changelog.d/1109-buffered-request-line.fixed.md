**daemon:** the daemon reads a request line a buffered block at a time and checks its 5 s deadline once per block instead of once per byte: a 64 KiB request is read in 50 µs instead of 909 µs.
