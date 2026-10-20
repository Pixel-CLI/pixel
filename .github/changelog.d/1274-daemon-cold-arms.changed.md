**daemon:** the request loop's error arms (over-long line, invalid UTF-8, timeout, unserializable reply) are marked cold, so the hot path no longer carries them inline.
