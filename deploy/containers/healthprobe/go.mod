// A module of its own, separate from server/gateway's.
//
// The probe is deployment machinery, not part of the gateway: putting it inside
// server/gateway would make it appear in that module's `go test ./...`, `go vet ./...`
// and dependency graph, and this work stream does not own that directory. Keeping it
// here also means the probe cannot accidentally acquire a dependency on gateway
// internals — it talks HTTP, like any other client, which is the point.
//
// Standard library only, and it should stay that way. A health probe with
// dependencies is a health probe with a supply chain.
module github.com/magickvoice/sentinel/deploy/healthprobe

// Matched to server/gateway/go.mod so both stages of the gateway image can be built
// by one toolchain image. If the gateway's Go version moves, move this with it.
go 1.25.0
