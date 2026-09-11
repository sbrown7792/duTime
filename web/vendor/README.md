# Vendored assets

Third-party libraries are committed here rather than fetched at build time, so
the binary builds identically on an air-gapped machine in five years' time.

| File | Version | License | Source |
|---|---|---|---|
| `echarts.min.js` | 5.5.1 | Apache-2.0 | https://cdn.jsdelivr.net/npm/echarts@5.5.1/dist/echarts.min.js |

## Updating

```console
$ curl -sSL -o echarts.min.js https://cdn.jsdelivr.net/npm/echarts@<version>/dist/echarts.min.js
$ sha256sum echarts.min.js          # record below
```

Then update the version in the table, and re-check the treemap and the stacked
area in both light and dark mode — ECharts occasionally changes label layout
and treemap gap behaviour between minor versions.

## Checksums

Recorded so a corrupted or substituted file is detectable.
    e84270bd0cd5bdf60fefc26d00c2a391cb2e81f4d26a7a9ee16185a54773a3cf  web/vendor/echarts.min.js

ECharts is distributed under the Apache License 2.0.
The full license text is included in the minified bundle header.
