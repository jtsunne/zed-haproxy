use zed_extension_api as zed;

struct HaproxyExtension;

impl zed::Extension for HaproxyExtension {
    fn new() -> Self {
        HaproxyExtension
    }
}

zed::register_extension!(HaproxyExtension);
