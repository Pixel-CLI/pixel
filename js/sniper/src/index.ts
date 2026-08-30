export { sniperDevPlugin, type SniperDevPluginOptions } from "./vite.ts";
export { SniperReporter, type SniperReporterOptions } from "./vitest-reporter.ts";
export {
  resolvePixelBin,
  SinkReporter,
  type ErrorEnvelope,
  type EventEnvelope,
  type EventKind,
  type Frame,
  type FramePackage,
  type HttpContext,
  type ReportEnvelope,
  type RunEnvelope,
  type Surface,
} from "./report.ts";
export {
  enrichFrames,
  parseEvaluatingChain,
  parseStack,
  ProvenanceTracker,
} from "./enrich.ts";
