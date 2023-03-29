use super::*;
use frame_support::{ pallet_prelude::DispatchResult};
use sp_std::convert::TryInto;
use sp_core::{H256, U256};
use crate::system::ensure_root;
use sp_io::hashing::sha2_256;
use sp_io::hashing::keccak_256;
use frame_system::{ensure_signed};
use sp_std::vec::Vec;
use substrate_fixed::types::I32F32;

const LOG_TARGET: &'static str = "runtime::subspace::registration";

impl<T: Config> Pallet<T> {


    // ---- The implementation for the extrinsic do_registration.
    //
    // # Args:
    // 	* 'origin': (<T as frame_system::Config>RuntimeOrigin):
    // 		- The signature of the calling key.
    //
    // 	* 'netuid' (u16):
    // 		- The u16 network identifier.
    //

    // 	* 'nonce' ( u64 ):
    // 		- Positive integer nonce used in POW.

    // 	* 'key' ( T::AccountId ):
    // 		- Key to be registered to the network.
    //
    // # Event:
    // 	* NeuronRegistered;
    // 		- On successfully registereing a uid to a neuron slot on a subnetwork.
    //
    // # Raises:
    // 	* 'NetworkDoesNotExist':
    // 		- Attempting to registed to a non existent network.
    //
    // 	* 'TooManyRegistrationsThisBlock':
    // 		- This registration exceeds the total allowed on this network this block.
    //
    // 	* 'AlreadyRegistered':
    // 		- The key is already registered on this network.
    //

    pub fn do_registration( 
        origin: T::RuntimeOrigin,
        netuid: u16
    ) -> DispatchResult {

        // --- 1. Check that the caller has signed the transaction. 
        // TODO( const ): This not be the key signature or else an exterior actor can register the key and potentially control it?
        let key = ensure_signed( origin )?;        
        log::info!("do_registration( key:{:?} netuid:{:?} )", key, netuid );

        // --- 2. Ensure the passed network is valid.
        ensure!( Self::if_subnet_exist( netuid ), Error::<T>::NetworkDoesNotExist ); 

        // --- 3. Ensure we are not exceeding the max allowed registrations per block.
        ensure!( Self::get_registrations_this_block( netuid ) < Self::get_max_registrations_per_block( netuid ), Error::<T>::TooManyRegistrationsThisBlock );

        // --- 4. Ensure that the key is not already registered.
        ensure!( !Uids::<T>::contains_key( netuid, &key ), Error::<T>::AlreadyRegistered );

        // --- 5. Ensure the passed block number is valid, not in the future or too old.
        // Work must have been done within 3 blocks (stops long range attacks).
        let current_block_number: u64 = Self::get_current_block_as_u64();

        // --- 9. Ensure that the key passes the registration requirement
        ensure!( Self::passes_network_connection_requirement( netuid, &key ), Error::<T>::DidNotPassConnectedNetworkRequirement );

        // --- 10. If the network account does not exist we will create it here.
        Self::create_account_if_non_existent( &key);         


        // --- 12. Append neuron or prune it.
        let subnetwork_uid: u16;
        let current_subnetwork_n: u16 = Self::get_subnetwork_n( netuid );

        // Possibly there is no neuron slots at all.
        ensure!( Self::get_max_allowed_uids( netuid ) != 0, Error::<T>::NetworkDoesNotExist );
        
        if current_subnetwork_n < Self::get_max_allowed_uids( netuid ) {

            // --- 12.1.1 No replacement required, the uid appends the subnetwork.
            // We increment the subnetwork count here but not below.
            subnetwork_uid = current_subnetwork_n;

            // --- 12.1.2 Expand subnetwork with new account.
            Self::append_neuron( netuid, &key );
            log::info!("add new neuron account");
        } else {
            // --- 12.1.1 Replacement required.
            // We take the neuron with the lowest pruning score here.
            subnetwork_uid = Self::get_neuron_to_prune( netuid );

            // --- 12.1.1 Replace the neuron account with the new info.
            Self::replace_neuron( netuid, subnetwork_uid, &key, current_block_number );
            log::info!("prune neuron");
        }

        // --- 14. Record the registration and increment block and interval counters.
        RegistrationsThisInterval::<T>::mutate( netuid, |val| *val += 1 );
        RegistrationsThisBlock::<T>::mutate( netuid, |val| *val += 1 );
    
        // --- 15. Deposit successful event.
        log::info!("NeuronRegistered( netuid:{:?} uid:{:?} key:{:?}  ) ", netuid, subnetwork_uid, key );
        Self::deposit_event( Event::NeuronRegistered( netuid, subnetwork_uid, key ) );

        // --- 16. Ok and done.
        Ok(())
    }

    // --- Checks if the key passes the topk prunning requirement in all connected networks.
    //
    pub fn passes_network_connection_requirement( netuid_a: u16, key: &T::AccountId ) -> bool {
        // --- 1. We are iterating over all networks to see if there is a registration connection.
        for (netuid_b, exists) in NetworksAdded::<T>::iter() {

            // --- 2. If the network exists and the registration connection requirement exists we will
            // check to see if we pass it.
            if exists && Self::network_connection_requirement_exists( netuid_a, netuid_b ){

                // --- 3. We cant be in the top percentile of an empty network.
                let subnet_n: u16 = Self::get_subnetwork_n( netuid_b );
                if subnet_n == 0 { return false; }

                // --- 4. First check to see if this key is already registered on this network.
                // If we are not registered we trivially fail the requirement.
                if !Self::is_key_registered_on_network( netuid_b, key ) { return false; }
                let uid_b: u16 = Self::get_uid_for_net_and_key( netuid_b, key ).unwrap();

                // --- 5. Next, count how many keys on the connected network have a better prunning score than
                // our target network.
                let mut n_better_prunning_scores: u16 = 0;
                let our_prunning_score_b: u16 = Self::get_pruning_score_for_uid( netuid_b, uid_b );
                for other_uid in 0..subnet_n {
                    let other_runing_score_b: u16 = Self::get_pruning_score_for_uid( netuid_b, other_uid );
                    if other_uid != uid_b && other_runing_score_b > our_prunning_score_b { n_better_prunning_scores = n_better_prunning_scores + 1; }
                }

                // --- 6. Using the n_better count we check to see if the target key is in the topk percentile.
                // The percentile is stored in NetworkConnect( netuid_i, netuid_b ) as a u16 normalized value (0, 1), 1 being top 100%.
                let topk_percentile_requirement: I32F32 = I32F32::from_num( Self::get_network_connection_requirement( netuid_a, netuid_b ) ) / I32F32::from_num( u16::MAX );
                let topk_percentile_value: I32F32 = I32F32::from_num( n_better_prunning_scores ) / I32F32::from_num( Self::get_subnetwork_n( netuid_b ) );
                if topk_percentile_value > topk_percentile_requirement { return false }
            }
        }

        return true;
    }

    pub fn vec_to_hash( vec_hash: Vec<u8> ) -> H256 {
        let de_ref_hash = &vec_hash; // b: &Vec<u8>
        let de_de_ref_hash: &[u8] = &de_ref_hash; // c: &[u8]
        let real_hash: H256 = H256::from_slice( de_de_ref_hash );
        return real_hash
    }

    // Determine which peer to prune from the network by finding the element with the lowest pruning score out of
    // immunity period. If all neurons are in immunity period, return node with lowest prunning score.
    // This function will always return an element to prune.
    pub fn get_neuron_to_prune(netuid: u16) -> u16 {
        let mut min_score : u16 = u16::MAX;
        let mut min_score_in_immunity_period = u16::MAX;
        let mut uid_with_min_score = 0;
        let mut uid_with_min_score_in_immunity_period: u16 =  0;
        if Self::get_subnetwork_n( netuid ) == 0 { return 0 } // If there are no neurons in this network.
        for neuron_uid_i in 0..Self::get_subnetwork_n( netuid ) {
            let pruning_score:u16 = Self::get_pruning_score_for_uid( netuid, neuron_uid_i );
            let block_at_registration: u64 = Self::get_neuron_block_at_registration( netuid, neuron_uid_i );
            let current_block :u64 = Self::get_current_block_as_u64();
            let immunity_period: u64 = Self::get_immunity_period(netuid) as u64;
            if min_score == pruning_score {
                if current_block - block_at_registration <  immunity_period { //neuron is in immunity period
                    if min_score_in_immunity_period > pruning_score {
                        min_score_in_immunity_period = pruning_score; 
                        uid_with_min_score_in_immunity_period = neuron_uid_i;
                    }
                }
                else {
                    min_score = pruning_score; 
                    uid_with_min_score = neuron_uid_i;
                }
            }
            // Find min pruning score.
            else if min_score > pruning_score { 
                if current_block - block_at_registration <  immunity_period { //neuron is in immunity period
                    if min_score_in_immunity_period > pruning_score {
                         min_score_in_immunity_period = pruning_score; 
                        uid_with_min_score_in_immunity_period = neuron_uid_i;
                    }
                }
                else {
                    min_score = pruning_score; 
                    uid_with_min_score = neuron_uid_i;
                }
            }
        }
        if min_score == u16::MAX { //all neuorns are in immunity period
            Self::set_pruning_score_for_uid( netuid, uid_with_min_score_in_immunity_period, u16::MAX );
            return uid_with_min_score_in_immunity_period;
        }
        else {
            // We replace the pruning score here with u16 max to ensure that all peers always have a 
            // pruning score. In the event that every peer has been pruned this function will prune
            // the last element in the network continually.
            Self::set_pruning_score_for_uid( netuid, uid_with_min_score, u16::MAX );
            return uid_with_min_score;
        }
    } 


    pub fn get_block_hash_from_u64 ( block_number: u64 ) -> H256 {
        let block_number: T::BlockNumber = TryInto::<T::BlockNumber>::try_into( block_number ).ok().expect("convert u64 to block number.");
        let block_hash_at_number: <T as frame_system::Config>::Hash = system::Pallet::<T>::block_hash( block_number );
        let vec_hash: Vec<u8> = block_hash_at_number.as_ref().into_iter().cloned().collect();
        let deref_vec_hash: &[u8] = &vec_hash; // c: &[u8]
        let real_hash: H256 = H256::from_slice( deref_vec_hash );

        log::trace!(
			target: LOG_TARGET,
			"block_number: {:?}, vec_hash: {:?}, real_hash: {:?}",
			block_number,
			vec_hash,
			real_hash
		);

        return real_hash;
    }

    pub fn hash_to_vec( hash: H256 ) -> Vec<u8> {
        let hash_as_bytes: &[u8] = hash.as_bytes();
        let hash_as_vec: Vec<u8> = hash_as_bytes.iter().cloned().collect();
        return hash_as_vec
    }

}