# TLS certificate boundary

No certificate or private-key file is stored in this directory. Modbus TLS
regression tests generate an ephemeral CA, server identity, and client identity
inside a temporary directory for each test and delete them automatically.

Production TLS identities must be provisioned by the user or deployment system
and selected explicitly at runtime. The build, installer, and signed-release
pipeline do not copy or generate a deployment identity.
