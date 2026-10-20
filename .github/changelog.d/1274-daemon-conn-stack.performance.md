**daemon:** connection threads get a 256 KiB stack instead of the 2 MiB default, which costs less to create and commits far less address space per connection.
