**graph:** a tree-sitter parse that runs past 3 s is cancelled and the file contributes no rows, instead of stalling the graph build on a malformed file (a 262-byte `.tsx` file hung it for minutes).
