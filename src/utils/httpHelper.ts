
import { storeTokens, getStoredTokens, loginCommand } from "../commands/login";

export function makeRequest(url: string, options: RequestInit = {}, _isRetry : boolean = false): Promise<Response> {
    console.log('Request URL:', process.env.FOREST_API_URL + url);

    return new Promise((resolve, reject) => {
        let tokens = getStoredTokens();

        fetch(process.env.FOREST_API_URL + url, {
            ...options,
            headers: {
                'Content-Type': 'application/json',
                'Authorization': `Bearer ${tokens.accessToken}`,
                ...options.headers,
            },
        }).then(async (response) => {
            if (response.status === 401 && !_isRetry) {
                
                console.log('Unauthorized request, attempting to refresh token...');

                fetch(process.env.FOREST_API_URL + 'v1/auth/refresh', {
                    method: 'POST',
                    body: JSON.stringify({
                        refreshToken: tokens.refreshToken,
                    })
                })
                .then(async refreshResponse => {
                    if (refreshResponse.status == 403) {
                        console.log('Refresh token is invalid or expired, please log in again.');
                        await loginCommand()
                        makeRequest(url, options, true);
                    }
                    return refreshResponse.json();
                })
                .then(data => {
                    
                    if (data.accessToken) {
                        console.log('Token refreshed successfully, retrying original request...');
                        storeTokens(data.accessToken, data.refreshToken);
                        return makeRequest(url, options, true);
                    } else {
                        console.error('Failed to refresh token:', data);
                        reject('Failed to refresh token');
                    }
                }).catch((error) => {
                    console.error('Error during token refresh:', error);
                    reject('Error during token refresh');
                });
            }

            if (!response.ok) {
                response.json()
                    .catch(() => {
                        reject(`Request failed with status ${response.status} and no JSON response.`);
                    })
                    .then(data => {
                        if (data && data.error) {
                            reject(data.error);
                        } else {
                            reject(`Request failed with status ${response.status} and no error message.`);
                        }
                    });

                return;
            }

            resolve(response);
        }).catch((error) => {
            console.error('Request error:', error);
            throw error;
        });
    });
}