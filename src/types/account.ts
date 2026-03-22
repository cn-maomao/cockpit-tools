export interface Account {
    id: string;
    email: string;
    name?: string;
    tags?: string[];
    notes?: string;
    token: TokenData;
    fingerprint_id?: string;
    quota?: QuotaData;
    quota_error?: QuotaErrorInfo;
    disabled?: boolean;
    disabled_reason?: string;
    disabled_at?: number;
    protected_models?: string[];
    created_at: number;
    last_used: number;
}

export interface TokenData {
    access_token: string;
    refresh_token: string;
    expires_in: number;
    expiry_timestamp: number;
    token_type: string;
    email?: string;
    project_id?: string;
    is_gcp_tos?: boolean;
    session_id?: string;
}

export interface QuotaData {
    models: ModelQuota[];
    last_updated: number;
    is_forbidden?: boolean;
    subscription_tier?: string;
    credits?: CreditInfo[];
    tier_id?: string;
}

export interface CreditInfo {
    credit_type: string;
    credit_amount?: string;
    minimum_credit_amount_for_usage?: string;
}

export interface QuotaErrorInfo {
    code?: number;
    message: string;
    timestamp: number;
}

export interface ModelQuota {
    name: string;
    display_name?: string;
    percentage: number;
    reset_time: string;
}

export interface DeviceProfile {
    machine_id: string;
    mac_machine_id: string;
    dev_device_id: string;
    sqm_id: string;
    service_machine_id?: string;
}

export interface DeviceProfileVersion {
    id: string;
    created_at: number;
    label: string;
    profile: DeviceProfile;
    is_current?: boolean;
}

export interface DeviceProfiles {
    current_storage?: DeviceProfile;
    bound_profile?: DeviceProfile;
    history: DeviceProfileVersion[];
    baseline?: DeviceProfile;
}

export interface RefreshStats {
    total: number;
    success: number;
    failed: number;
    details: string[];
}

// 指纹类型
export interface Fingerprint {
    id: string;
    name: string;
    profile: DeviceProfile;
    created_at: number;
}

export interface FingerprintWithStats extends Fingerprint {
    is_original: boolean;
    is_current: boolean;
    bound_account_count: number;
}
