// ISO 4217 List One, published 2026-09-17 by the maintenance agency (SIX).
// https://www.six-group.com/dam/download/financial-information/data-center/iso-currrency/lists/list-one.xml
// Collapse duplicate country rows and keep codes alphabetized for lookup.
// Keep previously supported codes and their scales when refreshing this catalog
// so earlier ledger amounts continue to mean the same thing.

use super::{Currency, Definition};

// Alphabetic code, numeric code, and minor-unit exponent.
pub(super) const CURRENCIES: &[Currency] = &[
    Currency(&Definition::registered("AED", 784, 2)),
    Currency(&Definition::registered("AFN", 971, 2)),
    Currency(&Definition::registered("ALL", 8, 2)),
    Currency(&Definition::registered("AMD", 51, 2)),
    Currency(&Definition::registered("AOA", 973, 2)),
    Currency(&Definition::registered("ARS", 32, 2)),
    Currency(&Definition::registered("AUD", 36, 2)),
    Currency(&Definition::registered("AWG", 533, 2)),
    Currency(&Definition::registered("AZN", 944, 2)),
    Currency(&Definition::registered("BAM", 977, 2)),
    Currency(&Definition::registered("BBD", 52, 2)),
    Currency(&Definition::registered("BDT", 50, 2)),
    Currency(&Definition::registered("BHD", 48, 3)),
    Currency(&Definition::registered("BIF", 108, 0)),
    Currency(&Definition::registered("BMD", 60, 2)),
    Currency(&Definition::registered("BND", 96, 2)),
    Currency(&Definition::registered("BOB", 68, 2)),
    Currency(&Definition::registered("BRL", 986, 2)),
    Currency(&Definition::registered("BSD", 44, 2)),
    Currency(&Definition::registered("BTN", 64, 2)),
    Currency(&Definition::registered("BWP", 72, 2)),
    Currency(&Definition::registered("BYN", 933, 2)),
    Currency(&Definition::registered("BZD", 84, 2)),
    Currency(&Definition::registered("CAD", 124, 2)),
    Currency(&Definition::registered("CDF", 976, 2)),
    Currency(&Definition::registered("CHF", 756, 2)),
    Currency(&Definition::registered("CLP", 152, 0)),
    Currency(&Definition::registered("CNY", 156, 2)),
    Currency(&Definition::registered("COP", 170, 2)),
    Currency(&Definition::registered("CRC", 188, 2)),
    Currency(&Definition::registered("CUP", 192, 2)),
    Currency(&Definition::registered("CVE", 132, 2)),
    Currency(&Definition::registered("CZK", 203, 2)),
    Currency(&Definition::registered("DJF", 262, 0)),
    Currency(&Definition::registered("DKK", 208, 2)),
    Currency(&Definition::registered("DOP", 214, 2)),
    Currency(&Definition::registered("DZD", 12, 2)),
    Currency(&Definition::registered("EGP", 818, 2)),
    Currency(&Definition::registered("ERN", 232, 2)),
    Currency(&Definition::registered("ETB", 230, 2)),
    Currency(&Definition::registered("EUR", 978, 2)),
    Currency(&Definition::registered("FJD", 242, 2)),
    Currency(&Definition::registered("FKP", 238, 2)),
    Currency(&Definition::registered("GBP", 826, 2)),
    Currency(&Definition::registered("GEL", 981, 2)),
    Currency(&Definition::registered("GHS", 936, 2)),
    Currency(&Definition::registered("GIP", 292, 2)),
    Currency(&Definition::registered("GMD", 270, 2)),
    Currency(&Definition::registered("GNF", 324, 0)),
    Currency(&Definition::registered("GTQ", 320, 2)),
    Currency(&Definition::registered("GYD", 328, 2)),
    Currency(&Definition::registered("HKD", 344, 2)),
    Currency(&Definition::registered("HNL", 340, 2)),
    Currency(&Definition::registered("HTG", 332, 2)),
    Currency(&Definition::registered("HUF", 348, 2)),
    Currency(&Definition::registered("IDR", 360, 2)),
    Currency(&Definition::registered("ILS", 376, 2)),
    Currency(&Definition::registered("INR", 356, 2)),
    Currency(&Definition::registered("IQD", 368, 3)),
    Currency(&Definition::registered("IRR", 364, 2)),
    Currency(&Definition::registered("ISK", 352, 0)),
    Currency(&Definition::registered("JMD", 388, 2)),
    Currency(&Definition::registered("JOD", 400, 3)),
    Currency(&Definition::registered("JPY", 392, 0)),
    Currency(&Definition::registered("KES", 404, 2)),
    Currency(&Definition::registered("KGS", 417, 2)),
    Currency(&Definition::registered("KHR", 116, 2)),
    Currency(&Definition::registered("KMF", 174, 0)),
    Currency(&Definition::registered("KPW", 408, 2)),
    Currency(&Definition::registered("KRW", 410, 0)),
    Currency(&Definition::registered("KWD", 414, 3)),
    Currency(&Definition::registered("KYD", 136, 2)),
    Currency(&Definition::registered("KZT", 398, 2)),
    Currency(&Definition::registered("LAK", 418, 2)),
    Currency(&Definition::registered("LBP", 422, 2)),
    Currency(&Definition::registered("LKR", 144, 2)),
    Currency(&Definition::registered("LRD", 430, 2)),
    Currency(&Definition::registered("LSL", 426, 2)),
    Currency(&Definition::registered("LYD", 434, 3)),
    Currency(&Definition::registered("MAD", 504, 2)),
    Currency(&Definition::registered("MDL", 498, 2)),
    Currency(&Definition::registered("MGA", 969, 2)),
    Currency(&Definition::registered("MKD", 807, 2)),
    Currency(&Definition::registered("MMK", 104, 2)),
    Currency(&Definition::registered("MNT", 496, 2)),
    Currency(&Definition::registered("MOP", 446, 2)),
    Currency(&Definition::registered("MRU", 929, 2)),
    Currency(&Definition::registered("MUR", 480, 2)),
    Currency(&Definition::registered("MVR", 462, 2)),
    Currency(&Definition::registered("MWK", 454, 2)),
    Currency(&Definition::registered("MXN", 484, 2)),
    Currency(&Definition::registered("MYR", 458, 2)),
    Currency(&Definition::registered("MZN", 943, 2)),
    Currency(&Definition::registered("NAD", 516, 2)),
    Currency(&Definition::registered("NGN", 566, 2)),
    Currency(&Definition::registered("NIO", 558, 2)),
    Currency(&Definition::registered("NOK", 578, 2)),
    Currency(&Definition::registered("NPR", 524, 2)),
    Currency(&Definition::registered("NZD", 554, 2)),
    Currency(&Definition::registered("OMR", 512, 3)),
    Currency(&Definition::registered("PAB", 590, 2)),
    Currency(&Definition::registered("PEN", 604, 2)),
    Currency(&Definition::registered("PGK", 598, 2)),
    Currency(&Definition::registered("PHP", 608, 2)),
    Currency(&Definition::registered("PKR", 586, 2)),
    Currency(&Definition::registered("PLN", 985, 2)),
    Currency(&Definition::registered("PYG", 600, 0)),
    Currency(&Definition::registered("QAR", 634, 2)),
    Currency(&Definition::registered("RON", 946, 2)),
    Currency(&Definition::registered("RSD", 941, 2)),
    Currency(&Definition::registered("RUB", 643, 2)),
    Currency(&Definition::registered("RWF", 646, 0)),
    Currency(&Definition::registered("SAR", 682, 2)),
    Currency(&Definition::registered("SBD", 90, 2)),
    Currency(&Definition::registered("SCR", 690, 2)),
    Currency(&Definition::registered("SDG", 938, 2)),
    Currency(&Definition::registered("SEK", 752, 2)),
    Currency(&Definition::registered("SGD", 702, 2)),
    Currency(&Definition::registered("SHP", 654, 2)),
    Currency(&Definition::registered("SLE", 925, 2)),
    Currency(&Definition::registered("SOS", 706, 2)),
    Currency(&Definition::registered("SRD", 968, 2)),
    Currency(&Definition::registered("SSP", 728, 2)),
    Currency(&Definition::registered("STN", 930, 2)),
    Currency(&Definition::registered("SVC", 222, 2)),
    Currency(&Definition::registered("SYP", 760, 2)),
    Currency(&Definition::registered("SZL", 748, 2)),
    Currency(&Definition::registered("THB", 764, 2)),
    Currency(&Definition::registered("TJS", 972, 2)),
    Currency(&Definition::registered("TMT", 934, 2)),
    Currency(&Definition::registered("TND", 788, 3)),
    Currency(&Definition::registered("TOP", 776, 2)),
    Currency(&Definition::registered("TRY", 949, 2)),
    Currency(&Definition::registered("TTD", 780, 2)),
    Currency(&Definition::registered("TWD", 901, 2)),
    Currency(&Definition::registered("TZS", 834, 2)),
    Currency(&Definition::registered("UAH", 980, 2)),
    Currency(&Definition::registered("UGX", 800, 0)),
    Currency(&Definition::registered("USD", 840, 2)),
    Currency(&Definition::registered("UYU", 858, 2)),
    Currency(&Definition::registered("UZS", 860, 2)),
    Currency(&Definition::registered("VED", 926, 2)),
    Currency(&Definition::registered("VES", 928, 2)),
    Currency(&Definition::registered("VND", 704, 0)),
    Currency(&Definition::registered("VUV", 548, 0)),
    Currency(&Definition::registered("WST", 882, 2)),
    Currency(&Definition::registered("XAF", 950, 0)),
    Currency(&Definition::registered("XCD", 951, 2)),
    Currency(&Definition::registered("XCG", 532, 2)),
    Currency(&Definition::registered("XOF", 952, 0)),
    Currency(&Definition::registered("XPF", 953, 0)),
    Currency(&Definition::registered("YER", 886, 2)),
    Currency(&Definition::registered("ZAR", 710, 2)),
    Currency(&Definition::registered("ZMW", 967, 2)),
    Currency(&Definition::registered("ZWG", 924, 2)),
];

// List One entries excluded from ledger money.
pub(super) const UNSUPPORTED_CODES: &[&str] = &[
    "BOV", // 984, fund with 2 minor-unit decimal places
    "CHE", // 947, fund with 2 minor-unit decimal places
    "CHW", // 948, fund with 2 minor-unit decimal places
    "CLF", // 990, fund with 4 minor-unit decimal places
    "COU", // 970, fund with 2 minor-unit decimal places
    "MXV", // 979, fund with 2 minor-unit decimal places
    "USN", // 997, fund with 2 minor-unit decimal places
    "UYI", // 940, fund with 0 minor-unit decimal places
    "UYW", // 927, fund with 4 minor-unit decimal places
    "XAD", // 396, fund with 2 minor-unit decimal places
    "XAG", // 961, no defined minor unit
    "XAU", // 959, no defined minor unit
    "XBA", // 955, no defined minor unit
    "XBB", // 956, no defined minor unit
    "XBC", // 957, no defined minor unit
    "XBD", // 958, no defined minor unit
    "XDR", // 960, no defined minor unit
    "XPD", // 964, no defined minor unit
    "XPT", // 962, no defined minor unit
    "XSU", // 994, no defined minor unit
    "XTS", // 963, no defined minor unit
    "XUA", // 965, no defined minor unit
    "XXX", // 999, no defined minor unit
];
