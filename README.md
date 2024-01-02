# lightning

torturing myself with LED lights

## HTTP API

GET `https://soulja-boy-told.me/light`, send query params `r` `g` `b` `bri`. all required between 0-255

## WebAssembly

POST `https://soulja-boy-told.me/light` send the WASM file as the body. API:

- `size() -> u32`
- `get_pixel(idx: u32) -> u32`
- `set_pixel(idx: u32, col: u32)`
- `time() -> u64`
  - returns unix time in ms

you export a `tick(time: f32)`. program runs for 30 seconds. colors in format `0xRRGGBB`, idx from zero to the return value of `size()`. compile to wasm32
