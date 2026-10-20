**daemon:** request lines are scanned with `memchr` and read into a buffer reused per connection, so a large request no longer costs a byte-at-a-time newline scan and a fresh allocation.
