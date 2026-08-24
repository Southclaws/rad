//! FNV-1a, 64-bit. A fixed specification, so digests written by one build
//! are readable by every other; the standard library's hasher deliberately
//! offers no such guarantee.

pub const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01b3;

pub fn fnv1a(bytes: &[u8]) -> u64 {
    fnv1a_from(OFFSET_BASIS, bytes)
}

/// Continue a digest, so a domain label and its payload hash as one stream
/// without joining them into a temporary buffer.
pub fn fnv1a_from(basis: u64, bytes: &[u8]) -> u64 {
    let mut hash = basis;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repeat_10(value: &[u8]) -> Vec<u8> {
        value.repeat(10)
    }

    fn repeat_500(value: &[u8]) -> Vec<u8> {
        value.repeat(500)
    }

    #[test]
    fn continuation_matches_hashing_the_joined_bytes() {
        let split = fnv1a_from(fnv1a(b"chongo "), b"was here!");
        assert_eq!(split, fnv1a(b"chongo was here!"));
    }

    #[test]
    fn reference_vectors() {
        assert_eq!(fnv1a(b""), OFFSET_BASIS);
        assert_eq!(fnv1a(b"a"), 0xaf63dc4c8601ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x85944171f73967e8);
        assert_eq!(
            fnv1a(b"http://www.isthe.com/chongo/index.html"),
            0x89aac3a491f0d729
        );
        assert_eq!(
            fnv1a(b"http://www.isthe.com/chongo/src/calc/lucas-calc"),
            0x32ce6b26e0f4a403
        );
        assert_eq!(
            fnv1a(b"http://www.isthe.com/chongo/tech/astro/venus2004.html"),
            0x614ab44e02b53e01
        );
        assert_eq!(
            fnv1a(b"http://www.isthe.com/chongo/tech/astro/vita.html"),
            0xfa6472eb6eef3290
        );
        assert_eq!(
            fnv1a(b"http://www.isthe.com/chongo/tech/comp/c/expert.html"),
            0x9e5d75eb1948eb6a
        );
        assert_eq!(
            fnv1a(b"http://www.isthe.com/chongo/tech/comp/calc/index.html"),
            0xb6d12ad4a8671852
        );
        assert_eq!(
            fnv1a(b"http://www.isthe.com/chongo/tech/comp/fnv/index.html"),
            0x88826f56eba07af1
        );
        assert_eq!(
            fnv1a(b"http://www.isthe.com/chongo/tech/math/number/howhigh.html"),
            0x44535bf2645bc0fd
        );
        assert_eq!(
            fnv1a(b"http://www.isthe.com/chongo/tech/math/number/number.html"),
            0x169388ffc21e3728
        );
        assert_eq!(
            fnv1a(b"http://www.isthe.com/chongo/tech/math/prime/mersenne.html"),
            0xf68aac9e396d8224
        );
        assert_eq!(
            fnv1a(b"http://www.isthe.com/chongo/tech/math/prime/mersenne.html#largest"),
            0x8e87d7e7472b3883
        );
        assert_eq!(
            fnv1a(b"http://www.lavarnd.org/cgi-bin/corpspeak.cgi"),
            0x295c26caa8b423de
        );
        assert_eq!(
            fnv1a(b"http://www.lavarnd.org/cgi-bin/haiku.cgi"),
            0x322c814292e72176
        );
        assert_eq!(
            fnv1a(b"http://www.lavarnd.org/cgi-bin/rand-none.cgi"),
            0x8a06550eb8af7268
        );
        assert_eq!(
            fnv1a(b"http://www.lavarnd.org/cgi-bin/randdist.cgi"),
            0xef86d60e661bcf71
        );
        assert_eq!(
            fnv1a(b"http://www.lavarnd.org/index.html"),
            0x9e5426c87f30ee54
        );
        assert_eq!(
            fnv1a(b"http://www.lavarnd.org/what/nist-test.html"),
            0xf1ea8aa826fd047e
        );
        assert_eq!(fnv1a(b"http://www.macosxhints.com/"), 0x0babaf9a642cb769);
        assert_eq!(fnv1a(b"http://www.mellis.com/"), 0x4b3341d4068d012e);
        assert_eq!(
            fnv1a(b"http://www.nature.nps.gov/air/webcams/parks/havoso2alert/havoalert.cfm"),
            0xd15605cbc30a335c
        );
        assert_eq!(
            fnv1a(b"http://www.nature.nps.gov/air/webcams/parks/havoso2alert/timelines_24.cfm"),
            0x5b21060aed8412e5
        );
        assert_eq!(fnv1a(b"http://www.paulnoll.com/"), 0x45e2cda1ce6f4227);
        assert_eq!(fnv1a(b"http://www.pepysdiary.com/"), 0x50ae3745033ad7d4);
        assert_eq!(
            fnv1a(b"http://www.sciencenews.org/index/home/activity/view"),
            0xaa4588ced46bf414
        );
        assert_eq!(
            fnv1a(b"http://www.skyandtelescope.com/"),
            0xc1b0056c4a95467e
        );
        assert_eq!(
            fnv1a(b"http://www.sput.nl/~rob/sirius.html"),
            0x56576a71de8b4089
        );
        assert_eq!(fnv1a(b"http://www.systemexperts.com/"), 0xbf20965fa6dc927e);
        assert_eq!(
            fnv1a(b"http://www.tq-international.com/phpBB3/index.php"),
            0x569f8383c2040882
        );
        assert_eq!(
            fnv1a(b"http://www.travelquesttours.com/index.htm"),
            0xe1e772fba08feca0
        );
        assert_eq!(
            fnv1a(b"http://www.wunderground.com/global/stations/89606.html"),
            0x4ced94af97138ac4
        );
        assert_eq!(fnv1a(&repeat_10(b"21701")), 0xc4112ffb337a82fb);
        assert_eq!(fnv1a(&repeat_10(b"M21701")), 0xd64a4fd41de38b7d);
        assert_eq!(fnv1a(&repeat_10(b"2^21701-1")), 0x4cfc32329edebcbb);
        assert_eq!(fnv1a(&repeat_10(b"\x54\xc5")), 0x0803564445050395);
        assert_eq!(fnv1a(&repeat_10(b"\xc5\x54")), 0xaa1574ecf4642ffd);
        assert_eq!(fnv1a(&repeat_10(b"23209")), 0x694bc4e54cc315f9);
        assert_eq!(fnv1a(&repeat_10(b"M23209")), 0xa3d7cb273b011721);
        assert_eq!(fnv1a(&repeat_10(b"2^23209-1")), 0x577c2f8b6115bfa5);
        assert_eq!(fnv1a(&repeat_10(b"\x5a\xa9")), 0xb7ec8c1a769fb4c1);
        assert_eq!(fnv1a(&repeat_10(b"\xa9\x5a")), 0x5d5cfce63359ab19);
        assert_eq!(fnv1a(&repeat_10(b"391581216093")), 0x33b96c3cd65b5f71);
        assert_eq!(fnv1a(&repeat_10(b"391581*2^216093-1")), 0xd845097780602bb9);
        assert_eq!(
            fnv1a(&repeat_10(b"\x05\xf9\x9d\x03\x4c\x81")),
            0x84d47645d02da3d5
        );
        assert_eq!(fnv1a(&repeat_10(b"FEDCBA9876543210")), 0x83544f33b58773a5);
        assert_eq!(
            fnv1a(&repeat_10(b"\xfe\xdc\xba\x98\x76\x54\x32\x10")),
            0x9175cbb2160836c5
        );
        assert_eq!(fnv1a(&repeat_10(b"EFCDAB8967452301")), 0xc71b3bc175e72bc5);
        assert_eq!(
            fnv1a(&repeat_10(b"\xef\xcd\xab\x89\x67\x45\x23\x01")),
            0x636806ac222ec985
        );
        assert_eq!(fnv1a(&repeat_10(b"0123456789ABCDEF")), 0xb6ef0e6950f52ed5);
        assert_eq!(
            fnv1a(&repeat_10(b"\x01\x23\x45\x67\x89\xab\xcd\xef")),
            0xead3d8a0f3dfdaa5
        );
        assert_eq!(fnv1a(&repeat_10(b"1032547698BADCFE")), 0x922908fe9a861ba5);
        assert_eq!(
            fnv1a(&repeat_10(b"\x10\x32\x54\x76\x98\xba\xdc\xfe")),
            0x6d4821de275fd5c5
        );
        assert_eq!(fnv1a(&repeat_500(b"\x00")), 0x1fe3fce62bd816b5);
        assert_eq!(fnv1a(&repeat_500(b"\x07")), 0xc23e9fccd6f70591);
        assert_eq!(fnv1a(&repeat_500(b"~")), 0xc1af12bdfe16b5b5);
        assert_eq!(fnv1a(&repeat_500(b"\x7f")), 0x39e9f18f2f85e221);
    }
}
