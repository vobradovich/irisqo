# Vendored UI assets

Compiled into the binary by `src/features/ui/assets.rs` and served under `/ui/assets/`.
The version is part of the file name, so the files are cached as immutable.

| File | Package | License | SHA-256 |
|---|---|---|---|
| `htmx-4.0.1.min.js` | [`htmx.org@4.0.1`](https://www.npmjs.com/package/htmx.org/v/4.0.1), `dist/htmx.min.js` | 0BSD | `a0abb04f2f45a1f89d1d4844023245537e5e6014c87b144553e0607b4cc32130` |
| `pico-2.1.1.classless.min.css` | [`@picocss/pico@2.1.1`](https://www.npmjs.com/package/@picocss/pico/v/2.1.1), `css/pico.classless.min.css` | MIT | `61207a40ffc02a42d1e50143651c121beab70ed413c934c1ff84fa263ba436b0` |

To update: take the file from the npm package, rename it with the new version, update the
names in `assets.rs` and `templates/ui/layout.html`, and this table.

Check a new file against the `integrity` of its npm tarball (`npm view htmx.org@X dist.integrity`):
the SRI hash printed in the htmx 4.0.1 README is the one of 4.0.0.
