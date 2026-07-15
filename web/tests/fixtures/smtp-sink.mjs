/**
 * A tiny SMTP sink for the e2e stack — accepts everything, delivers nothing. Arming the app's
 * five TOPOS_MAIL_SMTP_* toward this sink makes `mailDelivery().canSend` TRUE for the whole
 * suite (the invite form enables, sign-ups send verification mail, seats bind through the
 * mailbox round-trip) while the assertable copy of every message still lands in the app's own
 * dev outbox (`.outbox.jsonl`) — this process just answers the protocol so a real send never
 * throws. Line protocol only: 220 greeting, EHLO → capabilities (AUTH, no STARTTLS — so the
 * client never tries to upgrade), AUTH → 235, MAIL/RCPT → 250, DATA → 354 then 250 at the
 * dot, QUIT → 221.
 */
import { createServer } from "node:net";

const PORT = Number(process.env.SMTP_SINK_PORT ?? "2598");

const server = createServer((socket) => {
  let inData = false;
  let buffer = "";
  socket.write("220 e2e-sink ESMTP\r\n");
  socket.on("error", () => socket.destroy());
  socket.on("data", (chunk) => {
    buffer += chunk.toString("utf8");
    if (inData) {
      const end = buffer.indexOf("\r\n.\r\n");
      if (end === -1) {
        return;
      }
      buffer = buffer.slice(end + 5);
      inData = false;
      socket.write("250 ok: queued as e2e\r\n");
    }
    let nl = buffer.indexOf("\r\n");
    while (nl !== -1 && !inData) {
      const line = buffer.slice(0, nl);
      buffer = buffer.slice(nl + 2);
      const verb = line.slice(0, 4).toUpperCase();
      if (verb === "EHLO" || verb === "HELO") {
        socket.write("250-e2e-sink\r\n250 AUTH PLAIN LOGIN\r\n");
      } else if (verb === "AUTH") {
        socket.write("235 2.7.0 accepted\r\n");
      } else if (verb === "DATA") {
        inData = true;
        socket.write("354 go ahead\r\n");
      } else if (verb === "QUIT") {
        socket.write("221 bye\r\n");
        socket.end();
      } else {
        // MAIL FROM / RCPT TO / RSET / NOOP / anything else: accepted.
        socket.write("250 ok\r\n");
      }
      nl = buffer.indexOf("\r\n");
    }
  });
});

server.listen(PORT, "127.0.0.1", () => {
  console.warn(`smtp sink listening on 127.0.0.1:${PORT}`);
});
