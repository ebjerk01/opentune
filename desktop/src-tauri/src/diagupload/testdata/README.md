# Test-only TLS material

A self-signed certificate and private key for `localhost`, used only by the
mock HTTPS endpoint in `../tests.rs`. They protect nothing, are never loaded
outside tests, and the client accepts them only with the test/dev insecure
flag. Do not reuse them.
