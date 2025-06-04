
import { storeTokens, getStoredTokens, loginCommand } from "../commands/login";
import got, { OptionsOfJSONResponseBody, Response} from 'got';

function tryRefresh() {
    let tokens = getStoredTokens();

    return got.post(process.env.FOREST_API_URL + 'v1/auth/refresh', {
        json: {
            refreshToken : tokens.refreshToken,
        },
        responseType : 'json',
        throwHttpErrors: false,
    }).then(async (response) => {
        if (response.statusCode === 200) {
            const data = response.body as any;
            storeTokens(data.accessToken, data.refreshToken);
            console.log('Token refreshed successfully.');
            return data;
        } else if (response.statusCode === 401) {
            console.log('Refresh token is invalid or expired, please log in again.');
            await loginCommand()
            return(getStoredTokens());
        } else {
            console.error('Failed to refresh token:', response.body);
            throw new Error('Failed to refresh token');
        }
    })
}

export async function makeRequest(url: string, options: OptionsOfJSONResponseBody = {}, _isRetry : boolean = false): Promise<Response> {
    console.log('Request URL:', process.env.FOREST_API_URL + url);

    let tokens = getStoredTokens();
    
    const response = await got(process.env.FOREST_API_URL + url, {
        ...options,
        responseType : 'json',
        throwHttpErrors: false,
        headers: {
            'Authorization': `Bearer ${tokens.accessToken}`,
            ...options.headers,
        },
    })

    // Check if the response is unauthorized (401)
    if (response.statusCode === 401 && !_isRetry) {
        console.log('Unauthorized request, attempting to refresh token...');

        try {
            await tryRefresh();
        } catch (error) {
            console.error('Failed to refresh token:', error);
            
            throw new Error('Unauthorized request and failed to refresh token. Please log in again.');
        }
    }

    const responseBody = response.body as any
    if (!response.ok) {
        console.log("Request failed with status:", response.statusCode);

        if (responseBody && responseBody.error) {
            console.log("Error message:", responseBody);
            throw new Error(`Request failed with status ${response.statusCode}: ${responseBody.error}`);
        } else {

            throw new Error(`Request failed with status ${response.statusCode} and no error message.`);
        }
    }

    return responseBody;
}