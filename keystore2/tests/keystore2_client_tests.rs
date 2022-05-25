// Copyright 2022, The Android Open Source Project
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use nix::unistd::{getuid, Gid, Uid};
use rustutils::users::AID_USER_OFFSET;
use serde::{Deserialize, Serialize};

use android_hardware_security_keymint::aidl::android::hardware::security::keymint::{
    Digest::Digest, ErrorCode::ErrorCode, KeyPurpose::KeyPurpose, SecurityLevel::SecurityLevel,
};
use android_system_keystore2::aidl::android::system::keystore2::{
    CreateOperationResponse::CreateOperationResponse, Domain::Domain,
    IKeystoreOperation::IKeystoreOperation, ResponseCode::ResponseCode,
};

use keystore2_test_utils::authorizations;
use keystore2_test_utils::get_keystore_service;
use keystore2_test_utils::key_generations;
use keystore2_test_utils::key_generations::Error;
use keystore2_test_utils::run_as;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum TestOutcome {
    Ok,
    BackendBusy,
    InvalidHandle,
    OtherErr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct BarrierReached;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ForcedOp(pub bool);

/// Generate a EC_P256 key using given domain, namespace and alias.
/// Create an operation using the generated key and perform sample signing operation.
fn create_signing_operation(
    forced_op: ForcedOp,
    op_purpose: KeyPurpose,
    op_digest: Digest,
    domain: Domain,
    nspace: i64,
    alias: Option<String>,
) -> binder::Result<CreateOperationResponse> {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let key_metadata = key_generations::generate_ec_p256_signing_key(
        &sec_level, domain, nspace, alias, None, None,
    )
    .unwrap();

    sec_level.createOperation(
        &key_metadata.key,
        &authorizations::AuthSetBuilder::new().purpose(op_purpose).digest(op_digest),
        forced_op.0,
    )
}

/// Performs sample signing operation.
fn perform_sample_sign_operation(
    op: &binder::Strong<dyn IKeystoreOperation>,
) -> Result<(), binder::Status> {
    op.update(b"my message")?;
    let sig = op.finish(None, None)?;
    assert!(sig.is_some());
    Ok(())
}

/// Create new operation on child proc and perform simple operation after parent notification.
fn execute_op_run_as_child(
    target_ctx: &'static str,
    domain: Domain,
    nspace: i64,
    alias: Option<String>,
    auid: Uid,
    agid: Gid,
    forced_op: ForcedOp,
) -> run_as::ChildHandle<TestOutcome, BarrierReached> {
    unsafe {
        run_as::run_as_child(target_ctx, auid, agid, move |reader, writer| {
            let result = key_generations::map_ks_error(create_signing_operation(
                forced_op,
                KeyPurpose::SIGN,
                Digest::SHA_2_256,
                domain,
                nspace,
                alias,
            ));

            // Let the parent know that an operation has been started, then
            // wait until the parent notifies us to continue, so the operation
            // remains open.
            writer.send(&BarrierReached {});
            reader.recv();

            // Continue performing the operation after parent notifies.
            match &result {
                Ok(CreateOperationResponse { iOperation: Some(op), .. }) => {
                    match key_generations::map_ks_error(perform_sample_sign_operation(op)) {
                        Ok(()) => TestOutcome::Ok,
                        Err(Error::Km(ErrorCode::INVALID_OPERATION_HANDLE)) => {
                            TestOutcome::InvalidHandle
                        }
                        Err(e) => panic!("Error in performing op: {:#?}", e),
                    }
                }
                Ok(_) => TestOutcome::OtherErr,
                Err(Error::Rc(ResponseCode::BACKEND_BUSY)) => TestOutcome::BackendBusy,
                _ => TestOutcome::OtherErr,
            }
        })
        .expect("Failed to create an operation.")
    }
}

fn create_operations(
    target_ctx: &'static str,
    forced_op: ForcedOp,
    max_ops: i32,
) -> Vec<run_as::ChildHandle<TestOutcome, BarrierReached>> {
    let alias = format!("ks_op_test_key_{}", getuid());
    let base_gid = 99 * AID_USER_OFFSET + 10001;
    let base_uid = 99 * AID_USER_OFFSET + 10001;
    (0..max_ops)
        .into_iter()
        .map(|i| {
            execute_op_run_as_child(
                target_ctx,
                Domain::APP,
                key_generations::SELINUX_SHELL_NAMESPACE,
                Some(alias.to_string()),
                Uid::from_raw(base_uid + (i as u32)),
                Gid::from_raw(base_gid + (i as u32)),
                forced_op,
            )
        })
        .collect()
}

/// This test verifies that backend service throws BACKEND_BUSY error when all
/// operations slots are full. This test creates operations in child processes and
/// collects the status of operations performed in each child proc and determines
/// whether any child proc exited with error status.
#[test]
fn keystore2_backend_busy_test() {
    const MAX_OPS: i32 = 100;
    static TARGET_CTX: &str = "u:r:untrusted_app:s0:c91,c256,c10,c20";

    let mut child_handles = create_operations(TARGET_CTX, ForcedOp(false), MAX_OPS);

    // Wait until all child procs notifies us to continue,
    // so that there are definitely enough operations outstanding to trigger a BACKEND_BUSY.
    for ch in child_handles.iter_mut() {
        ch.recv();
    }
    // Notify each child to resume and finish.
    for ch in child_handles.iter_mut() {
        ch.send(&BarrierReached {});
    }

    // Collect the result and validate whether backend busy has occurred.
    let mut busy_count = 0;
    for ch in child_handles.into_iter() {
        if ch.get_result() == TestOutcome::BackendBusy {
            busy_count += 1;
        }
    }
    assert!(busy_count > 0)
}

/// This test confirms that forced operation is having high pruning power.
/// 1. Initially create regular operations such that there are enough operations outstanding
///    to trigger BACKEND_BUSY.
/// 2. Then, create a forced operation. System should be able to prune one of the regular
///    operations and create a slot for forced operation successfully.
#[test]
fn keystore2_forced_op_after_backendbusy_test() {
    const MAX_OPS: i32 = 100;
    static TARGET_CTX: &str = "u:r:untrusted_app:s0:c91,c256,c10,c20";

    // Create regular operations.
    let mut child_handles = create_operations(TARGET_CTX, ForcedOp(false), MAX_OPS);

    // Wait until all child procs notifies us to continue, so that there are enough
    // operations outstanding to trigger a BACKEND_BUSY.
    for ch in child_handles.iter_mut() {
        ch.recv();
    }

    // Create a forced operation.
    let auid = 99 * AID_USER_OFFSET + 10604;
    let agid = 99 * AID_USER_OFFSET + 10604;
    unsafe {
        run_as::run_as(
            key_generations::TARGET_VOLD_CTX,
            Uid::from_raw(auid),
            Gid::from_raw(agid),
            move || {
                let alias = format!("ks_prune_forced_op_key_{}", getuid());

                // To make room for this forced op, system should be able to prune one of the
                // above created regular operations and create a slot for this forced operation
                // successfully.
                create_signing_operation(
                    ForcedOp(true),
                    KeyPurpose::SIGN,
                    Digest::SHA_2_256,
                    Domain::SELINUX,
                    100,
                    Some(alias),
                )
                .expect("Client failed to create forced operation after BACKEND_BUSY state.");
            },
        );
    };

    // Notify each child to resume and finish.
    for ch in child_handles.iter_mut() {
        ch.send(&BarrierReached {});
    }

    // Collect the results of above created regular operations.
    let mut pruned_count = 0;
    let mut busy_count = 0;
    let mut _other_err = 0;
    for ch in child_handles.into_iter() {
        match ch.get_result() {
            TestOutcome::BackendBusy => {
                busy_count += 1;
            }
            TestOutcome::InvalidHandle => {
                pruned_count += 1;
            }
            _ => {
                _other_err += 1;
            }
        }
    }
    // Verify that there should be at least one backend busy has occurred while creating
    // above regular operations.
    assert!(busy_count > 0);

    // Verify that there should be at least one pruned operation which should have failed while
    // performing operation.
    assert!(pruned_count > 0);
}

/// This test confirms that forced operations can't be pruned.
///  1. Creates an initial forced operation and tries to complete the operation after BACKEND_BUSY
///     error is triggered.
///  2. Create MAX_OPS number of forced operations so that definitely enough number of operations
///     outstanding to trigger a BACKEND_BUSY.
///  3. Try to use initially created forced operation (in step #1) and able to perform the
///     operation successfully. This confirms that none of the later forced operations evicted the
///     initial forced operation.
#[test]
fn keystore2_max_forced_ops_test() {
    const MAX_OPS: i32 = 100;
    let auid = 99 * AID_USER_OFFSET + 10205;
    let agid = 99 * AID_USER_OFFSET + 10205;

    // Create initial forced operation in a child process
    // and wait for the parent to notify to perform operation.
    let alias = format!("ks_forced_op_key_{}", getuid());
    let mut first_op_handle = execute_op_run_as_child(
        key_generations::TARGET_SU_CTX,
        Domain::SELINUX,
        key_generations::SELINUX_SHELL_NAMESPACE,
        Some(alias),
        Uid::from_raw(auid),
        Gid::from_raw(agid),
        ForcedOp(true),
    );

    // Wait until above child proc notifies us to continue, so that there is definitely a forced
    // operation outstanding to perform a operation.
    first_op_handle.recv();

    // Create MAX_OPS number of forced operations.
    let mut child_handles =
        create_operations(key_generations::TARGET_SU_CTX, ForcedOp(true), MAX_OPS);

    // Wait until all child procs notifies us to continue, so that  there are enough operations
    // outstanding to trigger a BACKEND_BUSY.
    for ch in child_handles.iter_mut() {
        ch.recv();
    }

    // Notify initial created forced operation to continue performing the operations.
    first_op_handle.send(&BarrierReached {});

    // Collect initially created forced operation result and is expected to complete operation
    // successfully.
    let first_op_result = first_op_handle.get_result();
    assert_eq!(first_op_result, TestOutcome::Ok);

    // Notify each child to resume and finish.
    for ch in child_handles.iter_mut() {
        ch.send(&BarrierReached {});
    }

    // Collect the result and validate whether backend busy has occurred with MAX_OPS number
    // of forced operations.
    let busy_count = child_handles
        .into_iter()
        .map(|ch| ch.get_result())
        .filter(|r| *r == TestOutcome::BackendBusy)
        .count();
    assert!(busy_count > 0);
}