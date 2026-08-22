/**
 * Structured JSON logs to stderr — the plane's discipline in TypeScript. Fields carry route
 * TEMPLATES and ids, never raw paths, tokens, ciphertext, or bodies; the CoreEnv contract makes
 * the same promise to the engine, and this is the one sink both keep it at.
 */

export type LogLevel = "info" | "warn" | "error";

export type Logger = (
  level: LogLevel,
  message: string,
  fields?: Record<string, string | number>,
) => void;

export function stderrLogger(stream: { write(chunk: string): unknown } = process.stderr): Logger {
  return (level, message, fields) => {
    stream.write(
      `${JSON.stringify({ ts: new Date().toISOString(), level, message, ...fields })}\n`,
    );
  };
}
