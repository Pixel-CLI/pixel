**daemon:** the process now uses mimalloc instead of the system allocator, which matters on the multi-threaded index path and for a long-lived daemon.
