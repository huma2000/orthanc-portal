"""A fake mail server (SMTP) for the portal's tests and for tests/dev_env.sh: nothing is sent anywhere.

    python3 mock_smtp.py [port] [folder]        (default 18025, $MOCK_LOG_DIR/mail)

It accepts every email (no encryption; AUTH PLAIN / LOGIN accepted with any password) and saves each
one, exactly as received, as <folder>/<number>.eml (open it with any mail program), plus one JSON line
per email in <folder>/mail.log: {"file", "from", "to", "subject"}.
"""
import json, os, socketserver, sys
from email import message_from_bytes, policy

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 18025
FOLDER = sys.argv[2] if len(sys.argv) > 2 else os.path.join(os.environ.get("MOCK_LOG_DIR", "/tmp/portal-tests"), "mail")
os.makedirs(FOLDER, exist_ok=True)


class Session(socketserver.StreamRequestHandler):
    def say(self, line):
        self.wfile.write((line + "\r\n").encode())

    def handle(self):
        self.say("220 mock-smtp ready")
        sender, recipients = "", []
        while True:
            raw = self.rfile.readline()
            if not raw:
                return
            line = raw.decode(errors="replace").rstrip("\r\n")
            verb = line[:4].upper()
            if verb in ("EHLO", "HELO"):
                self.wfile.write(b"250-mock-smtp\r\n250-AUTH PLAIN LOGIN\r\n250 8BITMIME\r\n")
            elif verb == "AUTH":
                if line.upper().startswith("AUTH LOGIN"):
                    self.say("334 VXNlcm5hbWU6"); self.rfile.readline()
                    self.say("334 UGFzc3dvcmQ6"); self.rfile.readline()
                self.say("235 accepted")
            elif verb == "MAIL":
                sender, recipients = line.split(":", 1)[1].strip(), []
                self.say("250 OK")
            elif verb == "RCPT":
                recipients.append(line.split(":", 1)[1].strip().strip("<>"))
                self.say("250 OK")
            elif verb == "DATA":
                self.say("354 end with .")
                data = bytearray()
                while True:
                    chunk = self.rfile.readline()
                    if not chunk or chunk in (b".\r\n", b".\n"):
                        break
                    data += chunk[1:] if chunk.startswith(b"..") else chunk
                number = len([f for f in os.listdir(FOLDER) if f.endswith(".eml")]) + 1
                name = f"{number:04d}.eml"
                with open(os.path.join(FOLDER, name), "wb") as f:
                    f.write(bytes(data))
                subject = str(message_from_bytes(bytes(data), policy=policy.default)["Subject"] or "")
                with open(os.path.join(FOLDER, "mail.log"), "a", encoding="utf-8") as f:
                    f.write(json.dumps({"file": name, "from": sender, "to": recipients, "subject": subject}) + "\n")
                self.say("250 queued")
            elif verb == "RSET":
                sender, recipients = "", []
                self.say("250 OK")
            elif verb == "QUIT":
                self.say("221 bye")
                return
            else:
                self.say("250 OK")


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


if __name__ == "__main__":
    Server(("127.0.0.1", PORT), Session).serve_forever()
