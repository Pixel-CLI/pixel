// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT
// Entry point: `bun run dev` serves the page and the JSON API on one port.
import { defaultConfig, handlers, type Config } from "./api";
import index from "../public/index.html";

export const createServer = (cfg: Config, port: number) => {
  const h = handlers(cfg);
  return Bun.serve({
    port,
    hostname: "127.0.0.1",
    idleTimeout: 0, // SSE runs stay quiet for longer than the 10 s default
    routes: {
      "/": index,
      "/api/prompts": { GET: h.prompts, POST: h.addPrompt },
      "/api/bins": { GET: h.bins },
      "/api/brief": { POST: h.brief },
      "/api/file": { GET: h.file },
      "/api/run": { POST: h.run },
      "/api/abort": { POST: h.abort },
    },
    development: process.env.NODE_ENV !== "production",
  });
};

if (import.meta.main) {
  const port = Number(process.env.PORT ?? 4901);
  const cfg = defaultConfig();
  const server = createServer(cfg, port);
  console.log(`brief playground: http://127.0.0.1:${server.port}  (fixture ${cfg.repo})`);
}
