# assets/

Files here are loaded with the asset server, e.g.

```rust
let texture: Handle<ImageData> = ctx.resources.expect_mut::<AssetServer>().load("textures/crate.png");
```

* Desktop reads them from this directory (hot reload is on in debug builds: edit a file and it
  updates in the running game).
* `space3d build --target web` copies them to `dist/web/assets/` (fetched over HTTP).
* Android/iOS: bundle them with the app (see `android/README.md`, `ios/README.md`).

Supported: PNG/JPEG/WebP/… images, glTF/GLB and OBJ models.
