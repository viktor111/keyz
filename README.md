# keyz
Simple async key value store with rust and tokio

## CLI and clients
- [CLI](https://github.com/viktor111/keyz_cli.git)
- [rust](https://github.com/viktor111/keyz_rust_client) 
- dotnet - Work in progress...
- python - Work in progress...
- js/ts - Work in progress...
- java - Work in progress...

## Supported commands

- ```SET [key] [value]```
  - Sets key and value
- ```SET [key] [value] EX [seconds]```
  - Sets key and value but with expiration time in seconds
- ```GET [key]```
  - Gets the value set for the given key
- ```EXIN [key]```
  - Returns the seconds left before a key will expire
- ```DEL [key]```
  - Deletes a key and value
- ```CLOSE```
  - Closes the connection

## Examples using commands
- ```SET text some text blah```
  - Will set the ```text``` key with value ```some text blah```
- ```SET user:1 { "username": "testUsername", "password": "hashedandsecrurepass" }```
  - Again it will set the key ```user:1``` wit value ```{ "username": "testUsername", "password": "hashedandsecrurepass" }```
  - This time however we used key ```user:1``` because there might many users in your app
- ```SET user:1 { "username": "testUsername", "password": "hashedandsecrurepass" } EX 60```
  - Exact same thing as before but this time we added ```EX 60``` at the end so the user:1 key will expire in 60 seconds
- ```GET user:1```
  - Will give you the value of ```user:1``` key
  - However if the key is expired it will return ```null```
- ```EXIN user:1```
  - Will return the time left this key has before expiration in seconds
  - If already expired it will return null
  - If key has no expiration set return null
- ```DEL user:1```
  - Will attempt to delete the key ```user:1```
  - If deleted it will return the key name deleted back in this case it will return ```user:1```
  - If the key does not exist it will return null
  
  ## Features
  Features besides the base SET GET DEL
  - [x] Key expiration
  - [x] Command to show time left a key has before expiry in seconds
  - [ ] Password protection
  - [ ] Persistance
  - [ ] Monitoring
  - [ ] Data compression
  - [ ] Data partitioning
