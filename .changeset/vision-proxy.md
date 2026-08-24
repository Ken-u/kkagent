# Vision proxy for non-vision models

A model configured with `experimental_vision_proxy = true` can act as a shared
multimodal interface for primary models that declare no image input capability.

- When the active primary model is non-vision and a vision proxy is configured,
  image blocks are replaced with text descriptions produced by the proxy model
  before each request goes out.
- The replacement is **permanent in session history**: base64 image blocks are
  dropped, only the text description is kept, saving memory and context budget.
  Original file paths remain in the surrounding message text (user
  `<image-attached>` markers or ReadMediaFile tool results), so switching back
  to a vision model lets the agent re-read the source via ReadMediaFile.
- `ReadMediaFile` stays visible to non-vision primary models when a proxy is
  configured, instead of being hidden.
- Descriptions are cached by SHA-256 of the image payload, so repeated rounds
  with the same image cost one proxy call in total.
- The proxy model must itself declare an image input capability (`image_in`
  etc.) and cannot be the `default_model`. At most one proxy per config.
