**daemon:** each response leaves in one write from a buffer reused per connection, instead of a fresh allocation and a separate newline write.
