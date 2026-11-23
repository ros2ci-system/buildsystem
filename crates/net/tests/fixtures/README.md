# Test certificates

These files exist only so the tests can run a TLS server on localhost.

- `ca.pem`: a certificate authority made for the tests. Its private key was
  deleted when the files were generated, so it cannot sign anything else.
- `server.pem` and `server.key`: a certificate and private key for `localhost`
  and `127.0.0.1`, signed by that authority. **The key is public. Never use
  these outside the tests.**
- `other-ca.pem`: an unrelated authority, for checking that a server whose
  certificate it did not sign is refused.
