# alpha2-compose

Compiles alpha2 compose specs into project documents.

Part of [alpha2](https://github.com/alpha2-sh/alpha2), a media engine that renders timeline projects on the GPU. A compose spec describes a video by intent: sequences with transitions, stacks, grids, media fitted to their areas, captions, and components such as slideshows, title cards, lower thirds and comparison grids. `compile` turns it into an ordinary `project.v1` document that `alpha2-model` validates, without reading files or touching a GPU: what it needs to know about the media comes in as facts, which a binding gathers. The Python package `alpha2` exposes it as `alpha2.compose`.

- [Documentation](https://docs.rs/alpha2-compose)
- [Repository](https://github.com/alpha2-sh/alpha2)

## License

MIT, copyright Alpha2 contributors.
