use ascii::AsciiStr;

use error::*;
use codec::{EncodableInHeader, EncodeHeaderHandle};

pub use utils::DateTime;

impl EncodableInHeader for DateTime {

    fn encode(&self, handle: &mut EncodeHeaderHandle) -> Result<()> {
        let as_str = self.to_rfc2822();
        let ascii = unsafe { AsciiStr::from_ascii_unchecked( &*as_str ) };
        handle.write_str( ascii )?;
        Ok( () )
    }
}

#[cfg(test)]
mod test {
    use super::*;

    ec_test!{ simple, {
        DateTime::test_time( 45 )
    } => ascii => [
        Text "Tue,  6 Aug 2013 04:11:45 +0000"
    ]}
}