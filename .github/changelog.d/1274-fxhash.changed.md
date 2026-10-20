**graph:** the extractor, graph, rank, recall and daemon crates hash with `FxHasher` instead of the DOS-resistant default, which is the wrong trade for a local index read by one process.
