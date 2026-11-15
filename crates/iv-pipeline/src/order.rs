//! Which neighbours to prefetch, nearest first, biased towards the direction of travel.

/// Indices to prefetch around `index`, excluding `index` itself, at most `count`.
///
/// Moving forward (`direction > 0`) the order is +1, +2, −1, +3, +4, −2, …: two ahead for every
/// one behind, because the next images are the ones about to be asked for. `direction == 0`
/// (just opened) alternates evenly: +1, −1, +2, −2, …. With `wrap`, indices continue past the
/// ends; without, out-of-range neighbours are skipped. No index appears twice.
pub fn prefetch_order(index: usize, len: usize, direction: i32, count: usize, wrap: bool) -> Vec<usize> {
    let mut out = Vec::with_capacity(count);
    if len <= 1 || count == 0 {
        return out;
    }
    let forward: isize = if direction < 0 { -1 } else { 1 };
    let (ahead_per_round, behind_per_round) = if direction == 0 { (1, 1) } else { (2, 1) };
    let (mut ahead, mut behind) = (0isize, 0isize);

    let resolve = |offset: isize| -> Option<usize> {
        let t = index as isize + offset;
        if wrap {
            Some(t.rem_euclid(len as isize) as usize)
        } else {
            (0..len as isize).contains(&t).then_some(t as usize)
        }
    };

    // every index other than `index` is reachable within `len` rounds
    for _ in 0..len {
        for _ in 0..ahead_per_round {
            ahead += 1;
            if let Some(i) = resolve(ahead * forward) {
                if i != index && !out.contains(&i) {
                    out.push(i);
                }
            }
        }
        for _ in 0..behind_per_round {
            behind += 1;
            if let Some(i) = resolve(-behind * forward) {
                if i != index && !out.contains(&i) {
                    out.push(i);
                }
            }
        }
        if out.len() >= count || out.len() == len - 1 {
            break;
        }
    }
    out.truncate(count);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_favours_what_comes_next() {
        assert_eq!(prefetch_order(10, 100, 1, 6, false), vec![11, 12, 9, 13, 14, 8]);
    }

    #[test]
    fn backward_mirrors_forward() {
        assert_eq!(prefetch_order(10, 100, -1, 6, false), vec![9, 8, 11, 7, 6, 12]);
    }

    #[test]
    fn freshly_opened_alternates() {
        assert_eq!(prefetch_order(10, 100, 0, 4, false), vec![11, 9, 12, 8]);
    }

    #[test]
    fn edges_skip_without_wrap_and_continue_with_wrap() {
        assert_eq!(prefetch_order(0, 100, 1, 4, false), vec![1, 2, 3, 4]);
        assert_eq!(prefetch_order(99, 100, 1, 3, false), vec![98, 97, 96]);
        assert_eq!(prefetch_order(99, 100, 1, 3, true), vec![0, 1, 98]);
    }

    #[test]
    fn small_lists_never_repeat_or_include_the_current_index() {
        assert_eq!(prefetch_order(1, 3, 1, 10, true), vec![2, 0]);
        assert_eq!(prefetch_order(0, 1, 1, 10, true), Vec::<usize>::new());
        let all = prefetch_order(2, 5, 1, 10, false);
        let mut sorted = all.clone();
        sorted.sort();
        assert_eq!(sorted, vec![0, 1, 3, 4]);
    }
}
