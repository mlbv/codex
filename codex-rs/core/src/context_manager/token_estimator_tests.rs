use super::*;
use serde_json::json;

const MODEL: &str = "gpt-test";

fn item(value: serde_json::Value) -> ResponseItem {
    serde_json::from_value(value).unwrap()
}

fn reasoning(bytes: usize) -> ResponseItem {
    item(json!({"type":"reasoning","summary":[],"encrypted_content":"x".repeat(bytes)}))
}

fn estimate_history_tokens(items: &[ResponseItem], model: &str) -> i64 {
    let estimator = TokenEstimator::for_model(model);
    items.iter().map(|item| estimator.item(item)).sum()
}

#[test]
fn ciphertext_calibration_matches_held_out_reasoning_usage() {
    // Equal-frequency samples from a separate day's reasoning-token measurements.
    let samples = [
        (1784, 22),
        (1932, 11),
        (1976, 13),
        (2020, 16),
        (2040, 19),
        (2084, 22),
        (2124, 24),
        (2188, 30),
        (2252, 35),
        (2340, 47),
        (2404, 44),
        (2508, 52),
        (2616, 62),
        (2724, 66),
        (2828, 73),
        (2956, 78),
        (3108, 99),
        (3300, 107),
        (3468, 135),
        (3724, 142),
        (3960, 163),
        (4236, 183),
        (4556, 196),
        (4984, 233),
        (5412, 259),
        (6028, 313),
        (6604, 380),
        (7608, 448),
        (8248, 516),
        (8548, 516),
        (8696, 516),
        (9228, 516),
    ];
    let mut total = 0;
    let mut error = 0;
    let mut uncalibrated = 0;
    for (bytes, actual) in samples {
        let history = [reasoning(bytes)];
        let estimate = estimate_history_tokens(&history, "gpt-6.1-sol");
        error += (estimate - actual).abs();
        total += actual;
        uncalibrated += estimate_history_tokens(&history, MODEL);
    }
    assert!(
        error * 100 < total * 11,
        "absolute error {error}, tokens {total}"
    );
    assert!(uncalibrated > total * 2);
}

fn check_plaintext_samples(samples: &[([usize; 7], i64)], role: &str) {
    let mut total = 0;
    let mut error = 0;
    let mut native_error = 0;
    for &(
        [
            letters,
            digits,
            punctuation,
            spaces,
            runs,
            newlines,
            unicode_bytes,
        ],
        actual,
    ) in samples
    {
        // Preserve measured character counts without retaining the original text.
        let mut text = " ".repeat(spaces - runs);
        text.push_str(&" a".repeat(runs));
        text.push_str(&"a".repeat(letters - runs));
        text.push_str(&"1".repeat(digits));
        text.push_str(&".".repeat(punctuation));
        text.push_str(&"\n".repeat(newlines));
        let unicode_tail = if unicode_bytes % 2 == 1 {
            text.push('中');
            unicode_bytes - 3
        } else {
            unicode_bytes
        };
        text.push_str(&"é".repeat(unicode_tail / 2));
        let history = [item(json!({
            "type": "message", "role": role,
            "content": [{"type": if role == "user" { "input_text" } else { "output_text" }, "text": text}]
        }))];
        error += (estimate_history_tokens(&history, "gpt-6.1-sol") - actual).abs();
        native_error += (estimate_history_tokens(&history, MODEL) - actual).abs();
        total += actual;
    }
    assert!(
        error * 100 < total * 10,
        "absolute error {error}, tokens {total}"
    );
    assert!(
        error * 5 < native_error * 4,
        "calibrated {error}, native {native_error}"
    );
}

#[test]
fn plaintext_calibration_matches_held_out_output_usage() {
    check_plaintext_samples(
        &[
            ([20, 5, 6, 0, 0, 0, 0], 19),
            ([77, 13, 27, 1, 1, 1, 0], 42),
            ([107, 22, 40, 6, 6, 2, 0], 63),
            ([179, 5, 53, 8, 8, 1, 0], 78),
            ([190, 32, 58, 9, 9, 2, 0], 89),
            ([587, 107, 38, 0, 0, 0, 0], 97),
            ([640, 115, 41, 0, 0, 0, 0], 107),
            ([210, 20, 85, 19, 16, 1, 0], 114),
            ([678, 132, 50, 0, 0, 0, 0], 122),
            ([526, 0, 33, 79, 79, 6, 0], 132),
            ([404, 5, 76, 12, 12, 1, 0], 141),
            ([422, 10, 81, 16, 16, 1, 0], 148),
            ([293, 35, 107, 21, 21, 1, 0], 155),
            ([300, 49, 120, 13, 13, 3, 0], 165),
            ([300, 60, 121, 13, 13, 3, 0], 170),
            ([458, 25, 91, 29, 29, 1, 0], 177),
            ([403, 28, 120, 33, 33, 1, 0], 187),
            ([346, 43, 123, 42, 42, 1, 0], 198),
            ([433, 37, 123, 45, 45, 1, 6], 209),
            ([1204, 221, 51, 0, 0, 0, 0], 220),
            ([491, 40, 160, 33, 33, 1, 0], 233),
            ([707, 26, 131, 44, 44, 1, 0], 249),
            ([1082, 0, 99, 152, 152, 12, 0], 266),
            ([701, 18, 188, 59, 59, 1, 0], 290),
            ([811, 37, 174, 98, 89, 3, 6], 320),
            ([1132, 45, 127, 153, 153, 13, 6], 363),
            ([867, 2, 282, 277, 106, 2, 0], 421),
            ([1807, 13, 292, 253, 202, 1, 0], 555),
            ([1874, 51, 453, 91, 91, 1, 0], 738),
            ([2713, 34, 705, 926, 148, 76, 0], 1201),
            ([4067, 121, 1133, 2531, 414, 133, 0], 1933),
            ([6539, 112, 2293, 3840, 1097, 1, 0], 3544),
        ],
        "assistant",
    );
}

#[test]
fn plaintext_calibration_matches_held_out_input_growth() {
    check_plaintext_samples(
        &[
            ([22, 14, 1, 0, 0, 0, 0], 12),
            ([15, 21, 1, 0, 0, 0, 0], 12),
            ([16, 20, 1, 0, 0, 0, 0], 12),
            ([26, 2, 1, 0, 0, 0, 0], 12),
            ([21, 7, 1, 0, 0, 0, 0], 12),
            ([23, 5, 1, 0, 0, 0, 0], 12),
            ([11, 25, 1, 0, 0, 0, 0], 12),
            ([38, 33, 4, 4, 4, 1, 0], 23),
            ([43, 28, 4, 4, 4, 1, 0], 23),
            ([60, 6, 5, 4, 4, 3, 0], 24),
            ([62, 4, 5, 4, 4, 3, 0], 24),
            ([57, 2, 13, 0, 0, 0, 0], 24),
            ([114, 22, 35, 4, 4, 3, 0], 56),
            ([108, 36, 35, 4, 4, 3, 0], 57),
            ([155, 35, 38, 10, 10, 3, 0], 69),
            ([274, 105, 101, 16, 16, 3, 6], 143),
            ([469, 124, 256, 14, 14, 3, 0], 262),
            ([853, 195, 374, 124, 83, 3, 0], 517),
            ([1120, 201, 453, 638, 165, 3, 0], 662),
            ([1314, 507, 300, 237, 205, 3, 0], 904),
            ([3473, 265, 628, 1153, 442, 104, 0], 1347),
            ([3110, 534, 1113, 1303, 369, 3, 0], 1745),
            ([7289, 223, 926, 1064, 1064, 3, 51], 2256),
            ([8737, 758, 947, 1494, 1494, 82, 273], 2869),
            ([8161, 96, 2016, 3095, 1220, 3, 6], 3542),
            ([8248, 490, 3154, 1541, 287, 3, 0], 4101),
            ([10824, 88, 3461, 5076, 1432, 3, 60], 4916),
            ([13486, 437, 3903, 2714, 1301, 3, 0], 5807),
            ([22697, 42, 4284, 20, 20, 3, 6], 7030),
            ([27624, 1118, 2710, 4478, 4478, 3, 371], 8178),
            ([27403, 1547, 3097, 7520, 4100, 414, 213], 9381),
            ([28426, 1179, 5211, 5311, 4105, 8, 55], 10861),
        ],
        "user",
    );
}
