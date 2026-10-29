//! The two UI languages (Russian / English) and the one helper everything needs: pick a string by language.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Ru,
    En,
}
impl Lang {
    pub fn pick<'a>(self, ru: &'a str, en: &'a str) -> &'a str {
        if self == Lang::Ru {
            ru
        } else {
            en
        }
    }
}