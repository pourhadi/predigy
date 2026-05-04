//! FIX 4.4 tag numbers used by this crate. Defined as constants
//! rather than an enum so we can use them as `u32` literals in match
//! arms and serialiser code.

pub const BEGIN_STRING: u32 = 8;
pub const BODY_LENGTH: u32 = 9;
pub const MSG_TYPE: u32 = 35;
pub const MSG_SEQ_NUM: u32 = 34;
pub const SENDER_COMP_ID: u32 = 49;
pub const TARGET_COMP_ID: u32 = 56;
pub const SENDING_TIME: u32 = 52;
pub const CHECKSUM: u32 = 10;

// Logon (35=A) and session control:
pub const ENCRYPT_METHOD: u32 = 98;
pub const HEART_BT_INT: u32 = 108;
pub const TEST_REQ_ID: u32 = 112;
pub const RESET_SEQ_NUM_FLAG: u32 = 141;
pub const USERNAME: u32 = 553;
pub const PASSWORD: u32 = 554;
pub const RAW_DATA_LENGTH: u32 = 95;
pub const RAW_DATA: u32 = 96;

// NewOrderSingle (35=D) and OrderCancelRequest (35=F):
pub const CL_ORD_ID: u32 = 11;
pub const ORIG_CL_ORD_ID: u32 = 41;
pub const SYMBOL: u32 = 55;
pub const SIDE: u32 = 54;
pub const TRANSACT_TIME: u32 = 60;
pub const ORDER_QTY: u32 = 38;
pub const ORD_TYPE: u32 = 40;
pub const PRICE: u32 = 44;
pub const TIME_IN_FORCE: u32 = 59;

// ExecutionReport (35=8):
pub const ORDER_ID: u32 = 37;
pub const EXEC_ID: u32 = 17;
pub const EXEC_TYPE: u32 = 150;
pub const ORD_STATUS: u32 = 39;
pub const CUM_QTY: u32 = 14;
pub const LAST_QTY: u32 = 32;
pub const LAST_PX: u32 = 31;
pub const LEAVES_QTY: u32 = 151;
pub const TEXT: u32 = 58;

/// Body string for `BeginString` (tag 8).
pub const BEGIN_STRING_VALUE: &str = "FIX.4.4";

/// Side codes (tag 54).
pub const SIDE_BUY: &str = "1";
pub const SIDE_SELL: &str = "2";

/// OrdType codes (tag 40).
pub const ORD_TYPE_LIMIT: &str = "2";

/// TimeInForce codes (tag 59).
pub const TIF_DAY: &str = "0";
pub const TIF_GTC: &str = "1";
pub const TIF_IOC: &str = "3";
pub const TIF_FOK: &str = "4";

/// MsgType codes (tag 35).
pub const MSG_TYPE_HEARTBEAT: &str = "0";
pub const MSG_TYPE_TEST_REQUEST: &str = "1";
pub const MSG_TYPE_RESEND_REQUEST: &str = "2";
pub const MSG_TYPE_REJECT: &str = "3";
pub const MSG_TYPE_LOGOUT: &str = "5";
pub const MSG_TYPE_EXECUTION_REPORT: &str = "8";
pub const MSG_TYPE_ORDER_CANCEL_REJECT: &str = "9";
pub const MSG_TYPE_LOGON: &str = "A";
pub const MSG_TYPE_NEW_ORDER_SINGLE: &str = "D";
pub const MSG_TYPE_ORDER_CANCEL_REQUEST: &str = "F";

/// ExecType / OrdStatus codes (tags 150 / 39).
pub const EXEC_TYPE_NEW: &str = "0";
pub const EXEC_TYPE_PARTIAL_FILL: &str = "1"; // legacy; modern uses 'F' for trade
pub const EXEC_TYPE_FILL: &str = "F";
pub const EXEC_TYPE_CANCELED: &str = "4";
pub const EXEC_TYPE_REJECTED: &str = "8";
pub const ORD_STATUS_NEW: &str = "0";
pub const ORD_STATUS_PARTIALLY_FILLED: &str = "1";
pub const ORD_STATUS_FILLED: &str = "2";
pub const ORD_STATUS_CANCELED: &str = "4";
pub const ORD_STATUS_REJECTED: &str = "8";

/// Field separator: ASCII SOH = 0x01.
pub const SOH: u8 = 0x01;
