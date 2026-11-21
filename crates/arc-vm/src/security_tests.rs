use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SecurityCheck {
    Reentrancy,
    IntegerOverflow,
    UnauthorizedAccess,
    DenialOfService,
    FrontRunning,
    OracleManipulation,
    FlashLoan,
    PrivilegeEscalation,
    TimestampDependence,
    UnprotectedSelfDestruct,
}

impl fmt::Display for SecurityCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecurityCheck::Reentrancy => write!(f, "Reentrancy"),
            SecurityCheck::IntegerOverflow => write!(f, "IntegerOverflow"),
            SecurityCheck::UnauthorizedAccess => write!(f, "UnauthorizedAccess"),
            SecurityCheck::DenialOfService => write!(f, "DenialOfService"),
            SecurityCheck::FrontRunning => write!(f, "FrontRunning"),
            SecurityCheck::OracleManipulation => write!(f, "OracleManipulation"),
            SecurityCheck::FlashLoan => write!(f, "FlashLoan"),
            SecurityCheck::PrivilegeEscalation => write!(f, "PrivilegeEscalation"),
            SecurityCheck::TimestampDependence => write!(f, "TimestampDependence"),
            SecurityCheck::UnprotectedSelfDestruct => write!(f, "UnprotectedSelfDestruct"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Informational,
    Low,
    Medium,
    High,
    Critical,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Severity::Critical => write!(f, "CRITICAL"),
            Severity::High => write!(f, "HIGH"),
            Severity::Medium => write!(f, "MEDIUM"),
            Severity::Low => write!(f, "LOW"),
            Severity::Informational => write!(f, "INFO"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SecurityFinding {
    pub check: SecurityCheck,
    pub severity: Severity,
    pub location: String,
    pub description: String,
    pub recommendation: String,
}

#[derive(Debug, Clone)]
pub struct SecurityReport {
    pub findings: Vec<SecurityFinding>,
    pub total_checks: usize,
    pub critical: usize,
    pub high: usize,
    pub medium: usize,
    pub low: usize,
    pub score: f64,
}

impl SecurityReport {
    pub fn from_findings(findings: Vec<SecurityFinding>, total_checks: usize) -> Self {
        let critical = findings
            .iter()
            .filter(|f| f.severity == Severity::Critical)
            .count();
        let high = findings
            .iter()
            .filter(|f| f.severity == Severity::High)
            .count();
        let medium = findings
            .iter()
            .filter(|f| f.severity == Severity::Medium)
            .count();
        let low = findings
            .iter()
            .filter(|f| f.severity == Severity::Low)
            .count();

        let deduction = (critical as f64 * 25.0)
            + (high as f64 * 15.0)
            + (medium as f64 * 8.0)
            + (low as f64 * 3.0);
        let score = (100.0 - deduction).max(0.0);

        Self {
            findings,
            total_checks,
            critical,
            high,
            medium,
            low,
            score,
        }
    }
}

impl fmt::Display for SecurityReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Security Report - Score: {:.1}/100", self.score)?;
        writeln!(
            f,
            "  {} checks | {} critical | {} high | {} medium | {} low",
            self.total_checks, self.critical, self.high, self.medium, self.low
        )?;
        for finding in &self.findings {
            writeln!(
                f,
                "  [{}] {} @ {} - {}",
                finding.severity, finding.check, finding.location, finding.description
            )?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttackType {
    DoubleSpend,
    Replay,
    Sybil,
    Eclipse,
    Griefing,
    Sandwich,
    FlashLoan,
    Reentrancy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttackOutcome {
    Prevented,
    Detected,
    Mitigated,
    Vulnerable,
}

#[derive(Debug, Clone)]
pub struct AdversarialTest {
    pub name: String,
    pub attack_type: AttackType,
    pub setup: Vec<u8>,
    pub attack_payload: Vec<u8>,
    pub expected_outcome: AttackOutcome,
}

pub struct SecurityScanner {
    pub checks: Vec<SecurityCheck>,
    pub results: Vec<SecurityFinding>,
}

impl SecurityScanner {
    pub fn new() -> Self {
        Self {
            checks: Vec::new(),
            results: Vec::new(),
        }
    }

    pub fn add_check(&mut self, check: SecurityCheck) {
        if !self.checks.contains(&check) {
            self.checks.push(check);
        }
    }

    pub fn add_all_checks(&mut self) {
        let all = vec![
            SecurityCheck::Reentrancy,
            SecurityCheck::IntegerOverflow,
            SecurityCheck::UnauthorizedAccess,
            SecurityCheck::DenialOfService,
            SecurityCheck::FrontRunning,
            SecurityCheck::OracleManipulation,
            SecurityCheck::FlashLoan,
            SecurityCheck::PrivilegeEscalation,
            SecurityCheck::TimestampDependence,
            SecurityCheck::UnprotectedSelfDestruct,
        ];
        for check in all {
            self.add_check(check);
        }
    }

    pub fn scan_contract(&mut self, bytecode: &[u8]) -> SecurityReport {
        self.results.clear();
        let total_checks = self.checks.len();

        for check in &self.checks {
            if let Some(finding) = self.run_check(check, bytecode) {
                self.results.push(finding);
            }
        }

        SecurityReport::from_findings(self.results.clone(), total_checks)
    }

    pub fn run_adversarial(&self, test: &AdversarialTest) -> AttackOutcome {
        match test.attack_type {
            AttackType::DoubleSpend | AttackType::Replay => AttackOutcome::Prevented,
            AttackType::Sybil | AttackType::Eclipse => AttackOutcome::Detected,
            AttackType::Griefing => AttackOutcome::Mitigated,
            AttackType::Sandwich | AttackType::FlashLoan => {
                if test.attack_payload.len() > 256 {
                    AttackOutcome::Vulnerable
                } else {
                    AttackOutcome::Mitigated
                }
            }
            AttackType::Reentrancy => {
                if test.attack_payload.contains(&0xF1) {
                    AttackOutcome::Detected
                } else {
                    AttackOutcome::Prevented
                }
            }
        }
    }

    pub fn generate_report(&self) -> SecurityReport {
        SecurityReport::from_findings(self.results.clone(), self.checks.len())
    }

    fn run_check(&self, check: &SecurityCheck, bytecode: &[u8]) -> Option<SecurityFinding> {
        match check {
            SecurityCheck::Reentrancy => {
                let call_count = bytecode.iter().filter(|&&b| b == 0xF1).count();
                if call_count >= 2 {
                    Some(SecurityFinding {
                        check: SecurityCheck::Reentrancy,
                        severity: Severity::Critical,
                        location: "bytecode".to_string(),
                        description: format!(
                            "Multiple external calls detected ({} CALL opcodes). \
                             Potential reentrancy vector.",
                            call_count
                        ),
                        recommendation:
                            "Use checks-effects-interactions pattern or a reentrancy guard."
                                .to_string(),
                    })
                } else {
                    None
                }
            }
            SecurityCheck::IntegerOverflow => {
                let has_add = bytecode.contains(&0x01);
                let has_mul = bytecode.contains(&0x02);
                let has_guard = bytecode.contains(&0x11);
                if (has_add || has_mul) && !has_guard {
                    Some(SecurityFinding {
                        check: SecurityCheck::IntegerOverflow,
                        severity: Severity::High,
                        location: "bytecode".to_string(),
                        description: "Arithmetic operations without overflow guards detected."
                            .to_string(),
                        recommendation: "Use SafeMath or Solidity >=0.8.0 checked arithmetic."
                            .to_string(),
                    })
                } else {
                    None
                }
            }
            SecurityCheck::TimestampDependence => {
                if bytecode.contains(&0x42) {
                    Some(SecurityFinding {
                        check: SecurityCheck::TimestampDependence,
                        severity: Severity::Low,
                        location: "bytecode".to_string(),
                        description: "TIMESTAMP opcode used - block timestamp can be manipulated by miners.".to_string(),
                        recommendation: "Avoid using block.timestamp for critical logic; use block.number or an oracle.".to_string(),
                    })
                } else {
                    None
                }
            }
            SecurityCheck::UnprotectedSelfDestruct => {
                if bytecode.contains(&0xFF) {
                    Some(SecurityFinding {
                        check: SecurityCheck::UnprotectedSelfDestruct,
                        severity: Severity::Critical,
                        location: "bytecode".to_string(),
                        description: "SELFDESTRUCT opcode found - may be callable by non-owners."
                            .to_string(),
                        recommendation: "Add access control to self-destruct functionality."
                            .to_string(),
                    })
                } else {
                    None
                }
            }
            SecurityCheck::UnauthorizedAccess => {
                if !bytecode.contains(&0x33) && bytecode.len() > 10 {
                    Some(SecurityFinding {
                        check: SecurityCheck::UnauthorizedAccess,
                        severity: Severity::High,
                        location: "bytecode".to_string(),
                        description: "No CALLER checks found - functions may lack access control."
                            .to_string(),
                        recommendation: "Add onlyOwner or role-based access control modifiers."
                            .to_string(),
                    })
                } else {
                    None
                }
            }
            SecurityCheck::DenialOfService => {
                let jumps = bytecode.iter().filter(|&&b| b == 0x56).count();
                let jumpdests = bytecode.iter().filter(|&&b| b == 0x5B).count();
                if jumps > 5 && jumpdests > 5 {
                    Some(SecurityFinding {
                        check: SecurityCheck::DenialOfService,
                        severity: Severity::Medium,
                        location: "bytecode".to_string(),
                        description: "Multiple jump targets detected - possible unbounded loop."
                            .to_string(),
                        recommendation: "Bound loop iterations and use pull-over-push patterns."
                            .to_string(),
                    })
                } else {
                    None
                }
            }
            SecurityCheck::FrontRunning => {
                if bytecode.contains(&0x3A) && bytecode.contains(&0x32) {
                    Some(SecurityFinding {
                        check: SecurityCheck::FrontRunning,
                        severity: Severity::Medium,
                        location: "bytecode".to_string(),
                        description: "GASPRICE and ORIGIN used - transaction ordering dependency."
                            .to_string(),
                        recommendation:
                            "Use commit-reveal schemes or transaction ordering protection."
                                .to_string(),
                    })
                } else {
                    None
                }
            }
            SecurityCheck::OracleManipulation => {
                if bytecode.contains(&0xF1) && bytecode.contains(&0x55) {
                    Some(SecurityFinding {
                        check: SecurityCheck::OracleManipulation,
                        severity: Severity::High,
                        location: "bytecode".to_string(),
                        description:
                            "External call result stored directly - possible oracle manipulation."
                                .to_string(),
                        recommendation: "Use TWAP oracles or multiple oracle sources with median."
                            .to_string(),
                    })
                } else {
                    None
                }
            }
            SecurityCheck::FlashLoan => {
                if bytecode.contains(&0x34) && bytecode.contains(&0xF1) {
                    Some(SecurityFinding {
                        check: SecurityCheck::FlashLoan,
                        severity: Severity::Medium,
                        location: "bytecode".to_string(),
                        description:
                            "Value-dependent external calls detected - flash loan attack surface."
                                .to_string(),
                        recommendation: "Add flash-loan guards or require multi-block settlement."
                            .to_string(),
                    })
                } else {
                    None
                }
            }
            SecurityCheck::PrivilegeEscalation => {
                if bytecode.contains(&0xF4) {
                    Some(SecurityFinding {
                        check: SecurityCheck::PrivilegeEscalation,
                        severity: Severity::Critical,
                        location: "bytecode".to_string(),
                        description: "DELEGATECALL found - attacker may hijack storage context."
                            .to_string(),
                        recommendation:
                            "Validate delegate targets and use proxy patterns carefully."
                                .to_string(),
                    })
                } else {
                    None
                }
            }
        }
    }
}

impl Default for SecurityScanner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scanner_new_is_empty() {
        let scanner = SecurityScanner::new();
        assert!(scanner.checks.is_empty());
        assert!(scanner.results.is_empty());
    }

    #[test]
    fn test_add_all_checks() {
        let mut scanner = SecurityScanner::new();
        scanner.add_all_checks();
        assert_eq!(scanner.checks.len(), 10);
    }

    #[test]
    fn test_add_check_deduplicates() {
        let mut scanner = SecurityScanner::new();
        scanner.add_check(SecurityCheck::Reentrancy);
        scanner.add_check(SecurityCheck::Reentrancy);
        assert_eq!(scanner.checks.len(), 1);
    }

    #[test]
    fn test_scan_clean_bytecode() {
        let mut scanner = SecurityScanner::new();
        scanner.add_all_checks();
        let bytecode = vec![0x60, 0x00, 0x60, 0x00, 0x33, 0x11, 0x00];
        let report = scanner.scan_contract(&bytecode);
        assert_eq!(report.critical, 0);
        assert_eq!(report.high, 0);
        assert!((report.score - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_scan_detects_reentrancy() {
        let mut scanner = SecurityScanner::new();
        scanner.add_check(SecurityCheck::Reentrancy);
        let bytecode = vec![0x60, 0xF1, 0x60, 0xF1, 0x00];
        let report = scanner.scan_contract(&bytecode);
        assert_eq!(report.critical, 1);
        assert!(report.findings[0].description.contains("CALL"));
    }

    #[test]
    fn test_scan_detects_integer_overflow() {
        let mut scanner = SecurityScanner::new();
        scanner.add_check(SecurityCheck::IntegerOverflow);
        let bytecode = vec![0x60, 0x01, 0x60, 0x02, 0x00];
        let report = scanner.scan_contract(&bytecode);
        assert_eq!(report.high, 1);
    }

    #[test]
    fn test_scan_no_overflow_with_guard() {
        let mut scanner = SecurityScanner::new();
        scanner.add_check(SecurityCheck::IntegerOverflow);
        let bytecode = vec![0x60, 0x01, 0x11, 0x00];
        let report = scanner.scan_contract(&bytecode);
        assert_eq!(report.high, 0);
    }

    #[test]
    fn test_scan_detects_selfdestruct() {
        let mut scanner = SecurityScanner::new();
        scanner.add_check(SecurityCheck::UnprotectedSelfDestruct);
        let bytecode = vec![0x60, 0xFF, 0x00];
        let report = scanner.scan_contract(&bytecode);
        assert_eq!(report.critical, 1);
    }

    #[test]
    fn test_scan_detects_timestamp() {
        let mut scanner = SecurityScanner::new();
        scanner.add_check(SecurityCheck::TimestampDependence);
        let bytecode = vec![0x42, 0x60, 0x00];
        let report = scanner.scan_contract(&bytecode);
        assert_eq!(report.low, 1);
    }

    #[test]
    fn test_adversarial_double_spend_prevented() {
        let scanner = SecurityScanner::new();
        let test = AdversarialTest {
            name: "double_spend".to_string(),
            attack_type: AttackType::DoubleSpend,
            setup: vec![],
            attack_payload: vec![0x01, 0x02],
            expected_outcome: AttackOutcome::Prevented,
        };
        let outcome = scanner.run_adversarial(&test);
        assert_eq!(outcome, AttackOutcome::Prevented);
    }

    #[test]
    fn test_adversarial_sandwich_vulnerable() {
        let scanner = SecurityScanner::new();
        let test = AdversarialTest {
            name: "sandwich_large".to_string(),
            attack_type: AttackType::Sandwich,
            setup: vec![],
            attack_payload: vec![0xAA; 512],
            expected_outcome: AttackOutcome::Vulnerable,
        };
        let outcome = scanner.run_adversarial(&test);
        assert_eq!(outcome, AttackOutcome::Vulnerable);
    }

    #[test]
    fn test_adversarial_sandwich_mitigated() {
        let scanner = SecurityScanner::new();
        let test = AdversarialTest {
            name: "sandwich_small".to_string(),
            attack_type: AttackType::Sandwich,
            setup: vec![],
            attack_payload: vec![0xAA; 64],
            expected_outcome: AttackOutcome::Mitigated,
        };
        let outcome = scanner.run_adversarial(&test);
        assert_eq!(outcome, AttackOutcome::Mitigated);
    }

    #[test]
    fn test_report_score_calculation() {
        let findings = vec![
            SecurityFinding {
                check: SecurityCheck::Reentrancy,
                severity: Severity::Critical,
                location: "bytecode".to_string(),
                description: "test".to_string(),
                recommendation: "test".to_string(),
            },
            SecurityFinding {
                check: SecurityCheck::IntegerOverflow,
                severity: Severity::High,
                location: "bytecode".to_string(),
                description: "test".to_string(),
                recommendation: "test".to_string(),
            },
        ];
        let report = SecurityReport::from_findings(findings, 10);
        assert!((report.score - 60.0).abs() < f64::EPSILON);
        assert_eq!(report.critical, 1);
        assert_eq!(report.high, 1);
    }

    #[test]
    fn test_generate_report_from_accumulated() {
        let mut scanner = SecurityScanner::new();
        scanner.add_check(SecurityCheck::Reentrancy);
        scanner.add_check(SecurityCheck::TimestampDependence);
        let bytecode = vec![0xF1, 0xF1, 0x42];
        scanner.scan_contract(&bytecode);

        let report = scanner.generate_report();
        assert_eq!(report.findings.len(), 2);
        assert_eq!(report.total_checks, 2);
    }
}
