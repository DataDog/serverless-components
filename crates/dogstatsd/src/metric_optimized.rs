use ustr::Ustr;
use crate::constants;
use crate::errors::ParseError;

#[derive(Clone, Debug)]
pub struct SortedTagsOptimized {
    values: Vec<(Ustr, Ustr)>,
}

impl SortedTagsOptimized {
    #[cfg(target_arch = "x86_64")]
    pub fn parse_simd(tags_section: &str) -> Result<Self, ParseError> {
        use std::arch::x86_64::*;
        
        if tags_section.is_empty() {
            return Ok(Self { values: Vec::new() });
        }

        let bytes = tags_section.as_bytes();
        let len = bytes.len();
        
        unsafe {
            let mut comma_positions = Vec::with_capacity(128);
            let mut colon_positions = Vec::with_capacity(128);
            
            let comma_vec = _mm256_set1_epi8(b',' as i8);
            let colon_vec = _mm256_set1_epi8(b':' as i8);
            
            let mut i = 0;
            while i + 32 <= len {
                let chunk = _mm256_loadu_si256(bytes.as_ptr().add(i) as *const __m256i);
                
                let comma_mask = _mm256_cmpeq_epi8(chunk, comma_vec);
                let colon_mask = _mm256_cmpeq_epi8(chunk, colon_vec);
                
                let comma_bits = _mm256_movemask_epi8(comma_mask) as u32;
                let colon_bits = _mm256_movemask_epi8(colon_mask) as u32;
                
                for bit in 0..32 {
                    if comma_bits & (1 << bit) != 0 {
                        comma_positions.push(i + bit);
                    }
                    if colon_bits & (1 << bit) != 0 {
                        colon_positions.push(i + bit);
                    }
                }
                
                i += 32;
            }
            
            while i < len {
                if bytes[i] == b',' {
                    comma_positions.push(i);
                } else if bytes[i] == b':' {
                    colon_positions.push(i);
                }
                i += 1;
            }
            
            Self::parse_with_positions(tags_section, &comma_positions, &colon_positions)
        }
    }

    #[cfg(target_arch = "aarch64")]
    pub fn parse_neon(tags_section: &str) -> Result<Self, ParseError> {
        use std::arch::aarch64::*;
        
        if tags_section.is_empty() {
            return Ok(Self { values: Vec::new() });
        }

        let bytes = tags_section.as_bytes();
        let len = bytes.len();
        
        unsafe {
            let mut comma_positions = Vec::with_capacity(128);
            let mut colon_positions = Vec::with_capacity(128);
            
            let comma_vec = vdupq_n_u8(b',');
            let colon_vec = vdupq_n_u8(b':');
            
            let mut i = 0;
            while i + 16 <= len {
                let chunk = vld1q_u8(bytes.as_ptr().add(i));
                
                let comma_mask = vceqq_u8(chunk, comma_vec);
                let colon_mask = vceqq_u8(chunk, colon_vec);
                
                for j in 0..16 {
                    if vgetq_lane_u8(comma_mask, j) != 0 {
                        comma_positions.push(i + j);
                    }
                    if vgetq_lane_u8(colon_mask, j) != 0 {
                        colon_positions.push(i + j);
                    }
                }
                
                i += 16;
            }
            
            while i < len {
                if bytes[i] == b',' {
                    comma_positions.push(i);
                } else if bytes[i] == b':' {
                    colon_positions.push(i);
                }
                i += 1;
            }
            
            Self::parse_with_positions(tags_section, &comma_positions, &colon_positions)
        }
    }

    fn parse_with_positions(
        tags_section: &str,
        comma_positions: &[usize],
        colon_positions: &[usize],
    ) -> Result<Self, ParseError> {
        let total_tags = comma_positions.len() + 1;
        let mut parsed_tags = Vec::with_capacity(total_tags);
        
        let mut start = 0;
        let mut colon_idx = 0;
        
        for &comma_pos in comma_positions {
            if start < tags_section.len() {
                let tag_slice = &tags_section[start..comma_pos];
                
                while colon_idx < colon_positions.len() && colon_positions[colon_idx] < start {
                    colon_idx += 1;
                }
                
                if colon_idx < colon_positions.len() && 
                   colon_positions[colon_idx] > start && 
                   colon_positions[colon_idx] < comma_pos {
                    let colon_pos = colon_positions[colon_idx] - start;
                    let key = &tag_slice[..colon_pos];
                    let value = &tag_slice[colon_pos + 1..];
                    parsed_tags.push((Ustr::from(key), Ustr::from(value)));
                } else if !tag_slice.is_empty() {
                    parsed_tags.push((Ustr::from(tag_slice), Ustr::from("")));
                }
            }
            start = comma_pos + 1;
        }
        
        if start < tags_section.len() {
            let tag_slice = &tags_section[start..];
            
            while colon_idx < colon_positions.len() && colon_positions[colon_idx] < start {
                colon_idx += 1;
            }
            
            if colon_idx < colon_positions.len() && colon_positions[colon_idx] > start {
                let colon_pos = colon_positions[colon_idx] - start;
                let key = &tag_slice[..colon_pos];
                let value = &tag_slice[colon_pos + 1..];
                parsed_tags.push((Ustr::from(key), Ustr::from(value)));
            } else if !tag_slice.is_empty() {
                parsed_tags.push((Ustr::from(tag_slice), Ustr::from("")));
            }
        }
        
        parsed_tags.sort_unstable();
        parsed_tags.dedup();
        
        if parsed_tags.len() > constants::MAX_TAGS {
            return Err(ParseError::Raw(format!(
                "Too many tags, more than {c}",
                c = constants::MAX_TAGS
            )));
        }
        
        Ok(Self { values: parsed_tags })
    }

    pub fn parse_single_pass(tags_section: &str) -> Result<Self, ParseError> {
        if tags_section.is_empty() {
            return Ok(Self { values: Vec::new() });
        }

        let bytes = tags_section.as_bytes();
        let len = bytes.len();
        
        let mut parsed_tags = Vec::with_capacity(16);
        let mut start = 0;
        let mut i = 0;
        let mut colon_pos = None;
        
        while i <= len {
            if i == len || bytes[i] == b',' {
                if start < i {
                    if let Some(cp) = colon_pos {
                        let key = unsafe { std::str::from_utf8_unchecked(&bytes[start..cp]) };
                        let value = unsafe { std::str::from_utf8_unchecked(&bytes[cp + 1..i]) };
                        parsed_tags.push((Ustr::from(key), Ustr::from(value)));
                    } else {
                        let tag = unsafe { std::str::from_utf8_unchecked(&bytes[start..i]) };
                        parsed_tags.push((Ustr::from(tag), Ustr::from("")));
                    }
                }
                start = i + 1;
                colon_pos = None;
            } else if bytes[i] == b':' && colon_pos.is_none() {
                colon_pos = Some(i);
            }
            i += 1;
        }
        
        parsed_tags.sort_unstable();
        parsed_tags.dedup();
        
        if parsed_tags.len() > constants::MAX_TAGS {
            return Err(ParseError::Raw(format!(
                "Too many tags, more than {c}",
                c = constants::MAX_TAGS
            )));
        }
        
        Ok(Self { values: parsed_tags })
    }

    pub fn parse_with_hash_dedup(tags_section: &str) -> Result<Self, ParseError> {
        use std::collections::HashSet;
        
        if tags_section.is_empty() {
            return Ok(Self { values: Vec::new() });
        }
        
        let mut seen = HashSet::with_capacity(16);
        let mut parsed_tags = Vec::with_capacity(16);
        
        for part in tags_section.split(',').filter(|s| !s.is_empty()) {
            let (key, value) = if let Some(i) = part.find(':') {
                let k = Ustr::from(&part[..i]);
                let v = Ustr::from(&part[i + 1..]);
                (k, v)
            } else {
                (Ustr::from(part), Ustr::from(""))
            };
            
            if seen.insert((key, value)) {
                parsed_tags.push((key, value));
            }
        }
        
        if parsed_tags.len() > constants::MAX_TAGS {
            return Err(ParseError::Raw(format!(
                "Too many tags, more than {c}",
                c = constants::MAX_TAGS
            )));
        }
        
        parsed_tags.sort_unstable();
        Ok(Self { values: parsed_tags })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_methods_equivalence() {
        let test_cases = vec![
            "",
            "env:prod",
            "env:prod,service:web",
            "env:prod,service:web,version:1.0.0",
            "tag1,tag2,tag3",
            "env:prod,debug,service:web,trace",
            "env:prod,env:prod,service:web",
        ];

        for tags in test_cases {
            let single_pass = SortedTagsOptimized::parse_single_pass(tags).unwrap();
            let hash_dedup = SortedTagsOptimized::parse_with_hash_dedup(tags).unwrap();
            
            assert_eq!(single_pass.values, hash_dedup.values, 
                      "Results differ for input: {}", tags);
            
            #[cfg(target_arch = "x86_64")]
            {
                let simd = SortedTagsOptimized::parse_simd(tags).unwrap();
                assert_eq!(single_pass.values, simd.values, 
                          "SIMD results differ for input: {}", tags);
            }
            
            #[cfg(target_arch = "aarch64")]
            {
                let neon = SortedTagsOptimized::parse_neon(tags).unwrap();
                assert_eq!(single_pass.values, neon.values, 
                          "NEON results differ for input: {}", tags);
            }
        }
    }
}