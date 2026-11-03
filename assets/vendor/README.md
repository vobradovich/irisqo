# Vendored UI assets

Compiled into the binary by `src/features/ui/assets.rs` and served under `/ui/assets/`.
The version is part of the file name, so the files are cached as immutable.

| File | Package | License | SHA-256 |
|---|---|---|---|
| `htmx-2.0.8.min.js` | [`htmx.org@2.0.8`](https://www.npmjs.com/package/htmx.org/v/2.0.8), `dist/htmx.min.js` | 0BSD | `22283ef68cb7545914f0a88a1bdedc7256a703d1d580c1d255217d0a50d31313` |
| `pico-2.1.1.classless.min.css` | [`@picocss/pico@2.1.1`](https://www.npmjs.com/package/@picocss/pico/v/2.1.1), `css/pico.classless.min.css` | MIT | `61207a40ffc02a42d1e50143651c121beab70ed413c934c1ff84fa263ba436b0` |

To update: take the file from the npm package, rename it with the new version, update the
names in `assets.rs` and `templates/ui/layout.html`, and this table.
