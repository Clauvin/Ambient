# Common pitfalls

## Rendering

### My object with a random color is black sometimes

The `color` component is a `Vec4`. Using `rand::random` to populate it will
result in the `w`/alpha channel also being between 0 and 1, which means your
object may be black and/or disappear if the alpha is below the default alpha
cut-off.

To fix this, use a random `Vec3` for your color and then extend it to a `Vec4`:

```rust
let color = rand::random::<Vec3>().extend(1.0);
```

## Running

### Fails to start on Linux (Error in Surface::configure: parent device is lost)

If you're running Wayland, you may have to start ambient with: `WAYLAND_DISPLAY=wayland-1 ambient run`. See [this issue](https://github.com/gfx-rs/wgpu/issues/2519) for details.


### Runtime error: import `...` has the wrong type

This can occur when you have `.wasm` files in your `build` folder that are using an old version of the Ambient API. Delete the `build` folder and try again - this should force them to be regenerated.



### Failed to download "file address, starting with an ip address":error trying to connect: tcp connect error: *etc* (os error 10060)

This can happen if your anti-virus is blocking the connection to the ip address: try deactivating it, then run the ambient project again with 'ambient run'.
