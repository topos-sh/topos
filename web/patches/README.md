# `patches/` — pinned dependency fixes

Applied by `bun install` through `patchedDependencies` in `package.json`. A patch here is a last
resort: it is code this repo does not own, so each one names the exact defect, why it could not be
fixed at our own boundary, and the test that proves it stays fixed.

## `compression@1.8.1`

`@react-router/serve` mounts `compression` in front of this app. The middleware redirects a
response's `drain` listeners onto the compression stream (`res.on('drain', …)` → `stream.on`) so a
writer sees the COMPRESSED stream's backpressure — but it never proxied removal, so
`res.removeListener('drain', …)` went to the response, which never held the listener, and the
stream kept it forever. `res.once('drain', …)` is caught by the same asymmetry: node's
once-wrapper registers through the patched `on` and unregisters through `removeListener`.

The write loop underneath this app awaits a drain once per chunk a streamed response could not
take — `@react-router/express` hands the response body to `writeReadableStreamToWritable`, which
does exactly that pair per chunk — so a large document accumulated one permanent listener per
chunk and production logged `MaxListenersExceededWarning: 11 drain listeners added to [Gzip]`.

Both halves are inside dependencies — the app hands React Router a `Response` and never touches the
Node response object — so there is no seam of ours to fix it at. The patch adds the missing
`removeListener`/`off` proxy and nothing else.

Proved by `tests/unit/compression-drain.test.ts`, which drives the very module the production
server loads: the middleware's own add/remove symmetry, and then the whole serving path — real
gzip, real socket backpressure, a slow reader — where an unpatched install piles up hundreds of
drain listeners and a patched one holds at one.

## A composed build must carry this folder

`@topos/web` is composed by supersets, and a superset that runs its own `bun install` gets its own
`node_modules` — including its own copy of `compression`, which is what its server actually loads
at runtime. `patchedDependencies` is per-manifest and does not travel with an imported module, so
such a build must mirror the entry in ITS `package.json` and ship a copy of the patch file beside
its own lockfile. Without that the composed image runs the unpatched dependency no matter how
correct this tree is, and the only visible symptom is the warning line above in its logs. The
end-to-end case in `tests/unit/compression-drain.test.ts` is the check that tells the two apart —
run it in the tree whose `node_modules` the deployed server loads.
